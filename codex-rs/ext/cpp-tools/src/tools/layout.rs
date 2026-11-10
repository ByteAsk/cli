//! `cpp_layout` — struct layout facts for alignment reasoning.
//!
//! Reports each record's exact offsets, size, and alignment, plus padding holes
//! and cacheline straddles, so the model reasons about layout from facts instead
//! of intuition. Two backends:
//!
//! ```text
//!   rung 1  pahole (Linux, exact)   compile a temp .o with -g, read DWARF —
//!                                    exact holes incl. bitfields.
//!   rung 2  clang record-layout     -Xclang -fdump-record-layouts (portable).
//!                                    Offsets/size/align are exact; holes are
//!                                    computed ONLY between members whose sizes
//!                                    are PROVABLE (scalars/arrays/pointers) —
//!                                    anything else → "hole analysis: partial".
//!   rung 3  neither                 per-OS install message + remote fallback.
//! ```
//!
//! Layout facts are compiler/DWARF ground truth — never `[simulated]`. There is
//! no wrapper-TU retry: a type not completed in the given TU gets an honest ask
//! for a TU that uses it, not a synthetic include that could change
//! semantics.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;

use crate::probe::Probe;
use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;

const TOOL_NAME: &str = "cpp_layout";
const DEFAULT_CACHELINE: u64 = 64;

pub struct CppLayoutTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutArgs {
    file: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    flags: Option<String>,
    #[serde(default)]
    cacheline: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for CppLayoutTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "file".to_string(),
            JsonSchema::string(Some(
                "Path to the C/C++ source (or header) defining the type(s), relative to the \
                 workspace or absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "name".to_string(),
            JsonSchema::string(Some(
                "Optional record-name substring filter (e.g. \"Order\"). Omit to report every \
                 record in the file."
                    .to_string(),
            )),
        );
        properties.insert(
            "flags".to_string(),
            JsonSchema::string(Some(
                "Optional extra compiler flags, whitespace-separated (e.g. \"-std=c++20 -I include\"). \
                 A compile_commands.json entry for the file is used automatically when present."
                    .to_string(),
            )),
        );
        properties.insert(
            "cacheline".to_string(),
            JsonSchema::integer(Some(format!(
                "Cacheline size in bytes for straddle detection (default: {DEFAULT_CACHELINE})."
            ))),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Report C/C++ struct/class layout FACTS: exact field offsets, size, \
                alignment, padding holes, and cacheline straddles — so alignment/packing reasoning \
                uses ground truth, not guesses. Uses pahole (exact, Linux) or clang's record-layout \
                dump (portable). C/C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["file".to_string()]),
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl CppLayoutTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: LayoutArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let cacheline = args.cacheline.unwrap_or(DEFAULT_CACHELINE).max(1);
        let timeout = timeout_from(None);
        // Quote-aware: `-DMSG="a b"` and `-I "my inc"` are one flag each.
        let user_flags = super::sanitizers::shell_split(args.flags.as_deref().unwrap_or(""));
        let db_flags = compile_db_flags(&self.cwd, &args.file);
        let mut flags = db_flags;
        flags.extend(user_flags);
        // Absolute, so a file named `-layout.cpp` is never read as an option.
        let file = super::sanitizers::resolve_path(&self.cwd, &args.file)
            .display()
            .to_string();
        let lang = language(&args.file);

        // Rung 1: pahole (Linux, exact via DWARF).
        let mut pahole_note = None;
        let pahole_probe = crate::probe::probe("pahole", &["--version"], &self.cwd).await;
        if pahole_probe.is_available() {
            match self
                .pahole_layout(&args, &file, lang, &flags, cacheline, timeout)
                .await?
            {
                PaholeOutcome::Text(text) => {
                    let result = check_result::CheckResult::pass(
                        TOOL_NAME,
                        "Exact layout from the compiled program's debug information (pahole).",
                    );
                    return Ok(crate::report::report(TOOL_NAME, &args.file, text, result));
                }
                PaholeOutcome::Declined(note) => pahole_note = note,
            }
        }

        // Rung 2: clang record-layout dump (portable, provable-only holes).
        let clang = clang_binary(&self.cwd).await;
        if let Some(clang) = clang {
            match self
                .clang_layout(&clang, &args, &file, lang, &flags, cacheline, timeout)
                .await?
            {
                ClangOutcome::Records(text) => {
                    let result = clang_layout_result(&text);
                    return Ok(crate::report::report(TOOL_NAME, &args.file, text, result));
                }
                ClangOutcome::NoMatch => {
                    return Err(FunctionCallError::RespondToModel(no_match_message(
                        &args,
                        pahole_note.as_deref(),
                    )));
                }
                ClangOutcome::CompileFailed(err) => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "could not analyze {}: the compiler rejected it: {}",
                        args.file,
                        err.trim()
                    )));
                }
            }
        }

        // Rung 3: nothing available.
        Err(FunctionCallError::RespondToModel(unavailable_message(
            &pahole_probe,
        )))
    }

    async fn pahole_layout(
        &self,
        args: &LayoutArgs,
        file: &str,
        lang: &str,
        flags: &[String],
        cacheline: u64,
        timeout: std::time::Duration,
    ) -> Result<PaholeOutcome, FunctionCallError> {
        let clang = match clang_binary(&self.cwd).await {
            Some(c) => c,
            // No compiler to make the .o; fall to rung 2 (also needs clang).
            None => return Ok(PaholeOutcome::Declined(None)),
        };
        let obj = unique_tmp("byteask-layout.o");
        // `-x`: without it a header given to `-c` compiles to a precompiled
        // header, not an object, and pahole found nothing — 11 of 24 real
        // calls named a header. `-fno-eliminate-unused-debug-types`: a
        // header's own types are unused in its own translation unit and would
        // otherwise get no DWARF at all. The file itself is still the only
        // thing compiled (no synthesized wrapper TU).
        let mut argv: Vec<String> = ["-g", "-x", lang, "-c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // For a named lookup only: listing every record without a name would
        // then list every type of every included header (82 records, 81 from
        // system headers, measured), burying the file's own.
        if args.name.is_some() {
            argv.push("-fno-eliminate-unused-debug-types".to_string());
        }
        argv.extend(flags.iter().cloned());
        argv.push(file.to_string());
        argv.push("-o".to_string());
        argv.push(obj.to_string_lossy().to_string());
        let compiled = run_capped(&clang, &argv, &self.cwd, timeout).await?;
        if !compiled.ok() || !obj.exists() {
            let _ = std::fs::remove_file(&obj);
            // Fall through to the clang dump, which surfaces the compile error.
            return Ok(PaholeOutcome::Declined(None));
        }

        // pahole knows records only by their unqualified name.
        let wanted = args
            .name
            .as_deref()
            .map(|name| name.rsplit("::").next().unwrap_or(name));
        let mut pahole_args = Vec::new();
        if let Some(wanted) = wanted {
            pahole_args.push("-C".to_string());
            pahole_args.push(wanted.to_string());
        }
        pahole_args.push(obj.to_string_lossy().to_string());
        let run = run_capped("pahole", &pahole_args, &self.cwd, timeout).await?;
        let mut records = match run {
            RunOutcome::Finished { stdout, .. } => parse_pahole(&stdout),
            RunOutcome::TimedOut { .. } => Vec::new(),
        };
        let mut note = None;
        if let (Some(requested), Some(wanted)) = (&args.name, wanted) {
            if !records.is_empty() {
                let defs = self
                    .dwarf_records(&obj, |name| name == wanted, timeout)
                    .await;
                match qualify(&mut records, requested, &defs) {
                    Qualified::Labelled(label) => note = label,
                    Qualified::NotIt(reason) => {
                        let _ = std::fs::remove_file(&obj);
                        return Ok(PaholeOutcome::Declined(Some(reason)));
                    }
                }
            } else if !requested.contains("::") {
                // `name` is documented as a substring filter, but `pahole -C`
                // matches whole names only: "Order" never found OrderBook, and
                // the clang fallback cannot see a header's unused types — the
                // tool answered "no record named like Order". Find the whole
                // names in DWARF and ask pahole for those.
                let defs = self
                    .dwarf_records(&obj, |name| name.contains(wanted), timeout)
                    .await;
                let mut names: Vec<&str> = defs
                    .iter()
                    .map(|(qualified, _)| qualified.rsplit("::").next().unwrap_or(qualified))
                    .collect();
                names.sort_unstable();
                names.dedup();
                if !names.is_empty() && names.len() <= MAX_SUBSTRING_MATCHES {
                    let argv = vec![
                        "-C".to_string(),
                        names.join(","),
                        obj.to_string_lossy().to_string(),
                    ];
                    if let RunOutcome::Finished { stdout, .. } =
                        run_capped("pahole", &argv, &self.cwd, timeout).await?
                    {
                        records = parse_pahole(&stdout);
                    }
                    let mut notes = Vec::new();
                    for record in &mut records {
                        let unqualified = record.name.clone();
                        let same: Vec<(String, u64)> = defs
                            .iter()
                            .filter(|(q, _)| q.rsplit("::").next() == Some(unqualified.as_str()))
                            .cloned()
                            .collect();
                        if let Qualified::Labelled(Some(label)) =
                            qualify(std::slice::from_mut(record), &unqualified, &same)
                        {
                            notes.push(label);
                        }
                    }
                    note = (!notes.is_empty()).then(|| notes.join("\n"));
                }
            }
        }
        let _ = std::fs::remove_file(&obj);
        if records.is_empty() {
            return Ok(PaholeOutcome::Declined(None));
        }
        let rendered = render_records(&records, cacheline);
        Ok(PaholeOutcome::Text(match note {
            Some(note) => format!("{note}\n\n{rendered}"),
            None => rendered,
        }))
    }

    /// Every definition of a record named `name` in the object's DWARF, with
    /// its namespace-qualified name and size. Empty when readelf is missing or
    /// its listing was cut: no labels rather than guessed ones.
    async fn dwarf_records(
        &self,
        obj: &Path,
        keep: impl Fn(&str) -> bool,
        timeout: std::time::Duration,
    ) -> Vec<(String, u64)> {
        let argv = vec!["--debug-dump=info".to_string(), obj.display().to_string()];
        match run_capped("readelf", &argv, &self.cwd, timeout).await {
            Ok(RunOutcome::Finished {
                stdout,
                truncated: false,
                ..
            }) => parse_dwarf_records_where(&stdout, keep),
            _ => Vec::new(),
        }
    }

    /// The target's scalar sizes under these flags. Empty (every scalar
    /// unprovable, so hole analysis is partial) if the compiler will not say.
    async fn data_model(
        &self,
        clang: &str,
        lang: &str,
        flags: &[String],
        timeout: std::time::Duration,
    ) -> DataModel {
        let mut argv = vec!["-x".to_string(), lang.to_string()];
        argv.extend(flags.iter().cloned());
        argv.extend(["-dM".to_string(), "-E".to_string(), "-".to_string()]);
        match run_capped(clang, &argv, &self.cwd, timeout).await {
            Ok(RunOutcome::Finished {
                success: true,
                stdout,
                ..
            }) => DataModel::from_macros(&stdout),
            _ => DataModel::default(),
        }
    }

    async fn clang_layout(
        &self,
        clang: &str,
        args: &LayoutArgs,
        file: &str,
        lang: &str,
        flags: &[String],
        cacheline: u64,
        timeout: std::time::Duration,
    ) -> Result<ClangOutcome, FunctionCallError> {
        let model = self.data_model(clang, lang, flags, timeout).await;
        // Canonical types: `uint64_t`, `u32` and `size_t` arrive as the
        // builtin they name, so their size is provable.
        let mut argv: Vec<String> = [
            "-fsyntax-only",
            "-x",
            lang,
            "-Xclang",
            "-fdump-record-layouts",
            "-Xclang",
            "-fdump-record-layouts-canonical",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        argv.extend(flags.iter().cloned());
        argv.push(file.to_string());
        let run = run_capped(clang, &argv, &self.cwd, timeout).await?;
        match run {
            RunOutcome::TimedOut { secs } => Ok(ClangOutcome::CompileFailed(format!(
                "clang timed out after {secs}s"
            ))),
            RunOutcome::Finished {
                success,
                stdout,
                stderr,
                ..
            } => {
                let mut records = parse_clang_layouts(&stdout, &model);
                // The file goes to clang as an absolute path, so anonymous
                // records read `(anonymous at /abs/dir/types.h:7:24)`.
                let prefix = format!("{}/", self.cwd.display());
                for record in &mut records {
                    record.name = record.name.replace(&prefix, "");
                }
                if let Some(name) = &args.name {
                    records.retain(|r| r.name.contains(name.as_str()));
                }
                if records.is_empty() {
                    // A failed compile can still dump system-header records;
                    // "no record named like X" would then hide the error.
                    if success {
                        Ok(ClangOutcome::NoMatch)
                    } else {
                        Ok(ClangOutcome::CompileFailed(stderr))
                    }
                } else {
                    let rendered = render_records(&records, cacheline);
                    Ok(ClangOutcome::Records(if success {
                        rendered
                    } else {
                        let first_error = stderr
                            .lines()
                            .find(|line| line.contains("error:"))
                            .unwrap_or("")
                            .trim();
                        format!(
                            "note: {} did not compile cleanly; these are the records clang \
                             completed before it stopped. First error: {first_error}\n\n{rendered}",
                            args.file
                        )
                    }))
                }
            }
        }
    }
}

enum PaholeOutcome {
    Text(String),
    /// pahole could not answer; the reason, when there is one worth telling.
    Declined(Option<String>),
}

/// clang's record-layout dump gives exact offsets and sizes; the padding
/// analysis is exact only when every member's size could be proven.
fn clang_layout_result(text: &str) -> check_result::CheckResult {
    if text.contains("hole analysis: partial") {
        check_result::CheckResult::approximate(
            TOOL_NAME,
            "Offsets and sizes are exact, but some member sizes could not be proven, so the padding analysis is partial.",
        )
    } else {
        check_result::CheckResult::pass(TOOL_NAME, "Exact layout from clang's record-layout dump.")
    }
}

enum ClangOutcome {
    Records(String),
    NoMatch,
    CompileFailed(String),
}

// ============================ data model =================================

/// A member with its exact offset and (when provable) size, both in bytes.
#[derive(Debug, Clone, PartialEq)]
struct Member {
    name: String,
    offset: u64,
    /// `None` when the size is not provable from the available information
    /// (a bitfield, a nested/unknown type) — the honest "cannot say" case.
    size: Option<u64>,
}

/// A record's layout, from either backend.
#[derive(Debug, Clone, PartialEq)]
struct Record {
    name: String,
    size: u64,
    align: Option<u64>,
    members: Vec<Member>,
    source: &'static str,
}

// ============================ helpers ====================================

fn unique_tmp(prefix: &str) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// C for `.c` files, C++ for everything else (headers included). Compiled as
/// C++, valid C fails (`int class;`, an uncast `malloc`).
fn language(file: &str) -> &'static str {
    match Path::new(file).extension().and_then(|ext| ext.to_str()) {
        Some("c") => "c",
        _ => "c++",
    }
}

/// A substring that matches more record names than this is too vague to lay
/// them all out; the clang fallback (or a longer name) answers instead.
const MAX_SUBSTRING_MATCHES: usize = 20;

enum Qualified {
    /// pahole's record is the one asked for (renamed to its qualified name
    /// where DWARF made that certain), with a note when others share the name.
    Labelled(Option<String>),
    /// pahole's record is not, or cannot be shown to be, the one asked for.
    NotIt(String),
}

/// pahole prints no namespaces and ONE record per unqualified name: measured,
/// `pahole -C Entry` showed `kvcache::Entry` and silently hid `other::Entry`.
/// DWARF lists every record carrying the name; pahole's is identified among
/// them by its size.
fn qualify(records: &mut [Record], requested: &str, defs: &[(String, u64)]) -> Qualified {
    let Some(record) = records.first_mut() else {
        return Qualified::Labelled(None);
    };
    if defs.is_empty() {
        return Qualified::Labelled(None);
    }
    let list = defs
        .iter()
        .map(|(name, size)| format!("{name} ({size} B)"))
        .collect::<Vec<_>>()
        .join(", ");
    let same_size: Vec<&String> = defs
        .iter()
        .filter(|(_, size)| *size == record.size)
        .map(|(name, _)| name)
        .collect();
    if requested.contains("::") {
        let suffix = format!("::{requested}");
        let wanted: Vec<&(String, u64)> = defs
            .iter()
            .filter(|(name, _)| name == requested || name.ends_with(&suffix))
            .collect();
        return match (wanted.as_slice(), same_size.as_slice()) {
            ([(name, size)], [only]) if *size == record.size && *only == name => {
                record.name = name.clone();
                Qualified::Labelled(None)
            }
            _ => Qualified::NotIt(format!(
                "pahole prints one record per unqualified name and could not show {requested}; \
                 records with that name here: {list}"
            )),
        };
    }
    if let [(name, _)] = defs {
        record.name = name.clone();
        return Qualified::Labelled(None);
    }
    Qualified::Labelled(Some(match same_size.as_slice() {
        [name] => {
            record.name = (*name).clone();
            format!(
                "note: {} records are named {requested} here: {list}. pahole shows one per name; \
                 by its size this is {name}. Ask for another by its qualified name.",
                defs.len()
            )
        }
        _ => format!(
            "note: {} records are named {requested} here: {list}. pahole shows one per name, and \
             which one this is cannot be told from its size. Ask for a qualified name.",
            defs.len()
        ),
    }))
}

/// Every DEFINITION of a record named `name` in a `readelf --debug-dump=info`
/// listing, with its namespace-qualified name and byte size.
#[cfg(test)]
fn parse_dwarf_records(dump: &str, name: &str) -> Vec<(String, u64)> {
    parse_dwarf_records_where(dump, |candidate| candidate == name)
}

/// [`parse_dwarf_records`] for every record whose name `keep` accepts.
fn parse_dwarf_records_where(dump: &str, keep: impl Fn(&str) -> bool) -> Vec<(String, u64)> {
    struct Die {
        level: usize,
        scope: bool,
        record: bool,
        name: Option<String>,
        size: Option<u64>,
        declaration: bool,
    }
    fn finish(
        die: Die,
        keep: &impl Fn(&str) -> bool,
        scope: &mut Vec<(usize, String)>,
        found: &mut Vec<(String, u64)>,
    ) {
        if let (true, false, Some(name), Some(size)) =
            (die.record, die.declaration, die.name.as_deref(), die.size)
        {
            if keep(name) {
                let mut qualified: Vec<&str> = scope.iter().map(|(_, n)| n.as_str()).collect();
                qualified.push(name);
                found.push((qualified.join("::"), size));
            }
        }
        if die.scope {
            let label = die
                .name
                .unwrap_or_else(|| "(anonymous namespace)".to_string());
            scope.push((die.level, label));
        }
    }
    fn attribute(line: &str, key: &str) -> Option<String> {
        let rest = &line[line.find(key)? + key.len()..];
        let value = rest.split_once(": ")?.1.trim();
        // `(indexed string: 0x74): Entry`
        let value = match value.strip_prefix('(') {
            Some(_) => value.split_once("): ").map_or(value, |(_, v)| v),
            None => value,
        };
        Some(value.trim().to_string())
    }
    let mut found = Vec::new();
    let mut scope: Vec<(usize, String)> = Vec::new();
    let mut pending: Option<Die> = None;
    for line in dump.lines() {
        let trimmed = line.trim_start();
        // A DIE: ` <2><421>: Abbrev Number: 15 (DW_TAG_structure_type)`.
        let header = trimmed
            .strip_prefix('<')
            .and_then(|rest| rest.split_once('>'))
            .filter(|(_, tail)| tail.contains("Abbrev Number"))
            .and_then(|(level, tail)| Some((level.parse::<usize>().ok()?, tail)));
        if let Some((level, tail)) = header {
            if let Some(die) = pending.take() {
                finish(die, &keep, &mut scope, &mut found);
            }
            scope.retain(|(l, _)| *l < level);
            let tag = tail
                .rsplit_once('(')
                .map_or("", |(_, t)| t.trim_end_matches(')'));
            let record = matches!(
                tag,
                "DW_TAG_structure_type" | "DW_TAG_class_type" | "DW_TAG_union_type"
            );
            pending = Some(Die {
                level,
                scope: record || tag == "DW_TAG_namespace",
                record,
                name: None,
                size: None,
                declaration: false,
            });
            continue;
        }
        let Some(die) = pending.as_mut() else {
            continue;
        };
        if let Some(value) = attribute(trimmed, "DW_AT_name") {
            die.name = Some(value);
        } else if let Some(value) = attribute(trimmed, "DW_AT_byte_size") {
            die.size = value.parse().ok();
        } else if attribute(trimmed, "DW_AT_declaration").is_some() {
            die.declaration = true;
        }
    }
    if let Some(die) = pending.take() {
        finish(die, &keep, &mut scope, &mut found);
    }
    found
}

/// Prefer `clang++`, fall back to `clang`, whichever runs `--version`.
async fn clang_binary(cwd: &Path) -> Option<String> {
    for cand in ["clang++", "clang"] {
        if crate::probe::probe(cand, &["--version"], cwd)
            .await
            .is_available()
        {
            return Some(cand.to_string());
        }
    }
    None
}

fn no_match_message(args: &LayoutArgs, pahole_note: Option<&str>) -> String {
    let pahole = pahole_note
        .map(|note| format!(" ({note}.)"))
        .unwrap_or_default();
    match &args.name {
        Some(name) => format!(
            "no record named like \"{name}\" was laid out in {}. clang's layout dump covers only \
             records the file itself uses (a header's own types usually are not), and cpp_layout \
             does NOT synthesize an include (that could change semantics): pass a source file \
             that uses or instantiates the type.{pahole}",
            args.file
        ),
        None => format!(
            "no record layout was produced for {}. If the types are only forward-declared there, \
             pass a TU that completes them.{pahole}",
            args.file
        ),
    }
}

fn unavailable_message(pahole: &Probe) -> String {
    let os = std::env::consts::OS;
    let why = pahole
        .describe()
        .unwrap_or_else(|| "no layout backend produced output".to_string());
    format!(
        "struct-layout analysis is unavailable on {os}: {why}, and no clang was found. Install \
         clang (LLVM / your OS's C++ toolchain) for the portable backend, or pahole (apt install \
         dwarves) on Linux for exact DWARF layout. As a remote fallback you can compile the file \
         with cpp_godbolt using `-Xclang -fdump-record-layouts`."
    )
}

// ----- clang record-layout dump parsing ----------------------------------

/// Parse `clang -Xclang -fdump-record-layouts` output into records. Offsets and
/// the size/align trailer are exact; member sizes are inferred conservatively
/// ([`member_size`]) so a wrong hole can never be reported.
fn parse_clang_layouts(stdout: &str, model: &DataModel) -> Vec<Record> {
    // Pass 1: every record's name, sizeof and dsize, so a member of record
    // type (or a base class) can be sized from its own dump.
    let raw = raw_clang_records(stdout);
    let sizes: HashMap<&str, (u64, u64)> = raw
        .iter()
        .map(|r| (r.name.as_str(), (r.size, r.dsize)))
        .collect();
    raw.iter()
        .map(|r| Record {
            name: r.name.clone(),
            size: r.size,
            align: r.align,
            members: clang_members(&r.lines, model, &sizes),
            source: "clang record layout",
        })
        .collect()
}

/// One `*** Dumping AST Record Layout` block, before its members are sized.
struct RawRecord {
    name: String,
    size: u64,
    /// Size without tail padding: what a base subobject of this type occupies.
    dsize: u64,
    align: Option<u64>,
    /// The record's DIRECT member lines (`<offset> | <decl>` one level deep).
    lines: Vec<(String, String)>,
}

fn raw_clang_records(stdout: &str) -> Vec<RawRecord> {
    let mut records = Vec::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        if !line.contains("Dumping AST Record Layout") {
            continue;
        }
        // Header: "<offset> | struct/class/union <Name>".
        let Some(name) = lines.next().and_then(parse_record_header) else {
            continue;
        };
        let mut record = RawRecord {
            name,
            size: 0,
            dsize: 0,
            align: None,
            lines: Vec::new(),
        };
        while let Some(peek) = lines.peek() {
            if peek.contains("Dumping AST Record Layout") {
                break;
            }
            let Some(cur) = lines.next() else { break };
            if cur.contains("sizeof=") {
                record.size = extract_kv(cur, "sizeof=").unwrap_or(0);
                record.dsize = extract_kv(cur, "dsize=").unwrap_or(record.size);
                record.align = extract_kv(cur, "align=");
                continue;
            }
            let Some((offset, decl)) = cur.split_once('|') else {
                continue;
            };
            // Depth is the indent after the bar: the header has one space, a
            // direct member three, its own members (a base class's, a nested
            // struct's) five or more. Only direct members belong to this
            // record; counting the deeper lines too attributed a base's and a
            // nested struct's fields to the outer record.
            let indent = decl.len() - decl.trim_start().len();
            if indent != 3 || offset.trim().is_empty() {
                continue;
            }
            record
                .lines
                .push((offset.trim().to_string(), decl.trim().to_string()));
        }
        records.push(record);
    }
    records
}

/// Parse the record header line `<offset> | struct/class/union <Name>`.
fn parse_record_header(line: &str) -> Option<String> {
    let after = line.split('|').nth(1)?.trim();
    // An empty record's header reads `struct Empty (empty)`.
    let after = after.strip_suffix(" (empty)").unwrap_or(after);
    for kw in ["struct ", "class ", "union "] {
        if let Some(rest) = after.strip_prefix(kw) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Turn a record's direct member lines into members, sized exactly or not at
/// all: vtable pointers and base subobjects are members too (skipping them
/// counted their bytes as padding: a class with a vptr and an int read "12 of
/// 16 bytes are padding (75%)"), and a run of bitfields is one member spanning
/// the bytes its bits occupy.
fn clang_members(
    lines: &[(String, String)],
    model: &DataModel,
    records: &HashMap<&str, (u64, u64)>,
) -> Vec<Member> {
    let mut members: Vec<Member> = Vec::new();
    // The byte range and names of the bitfield run being accumulated.
    let mut bits: Option<(u64, u64, String)> = None;
    for (offset, decl) in lines {
        if let Some((byte, range)) = offset.split_once(':') {
            let Ok(byte) = byte.parse::<u64>() else {
                continue;
            };
            let name = member_name(decl);
            // `80:3-9` = bits 3..=9 counted from byte 80. Like pahole, a run
            // of bitfields occupies whole storage units of its declared type
            // (an `int` field lives in a 4-byte unit), so both backends agree
            // on the padding. Older clang printed just `80:3`, with no extent,
            // and an unsized type gives no unit: the size is then not provable.
            let unit = type_size(decl_type(decl), model, records).filter(|&unit| unit > 0);
            let span = range
                .split_once('-')
                .zip(unit)
                .and_then(|((first, last), unit)| {
                    let (first, last) = (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?);
                    let (first_bit, last_bit) = (byte * 8 + first, byte * 8 + last);
                    let start = first_bit / (unit * 8) * unit;
                    let end = (last_bit / (unit * 8) + 1) * unit;
                    Some((start, end, last + 1 - first))
                });
            let Some((start, end, width)) = span else {
                flush_bits(&mut bits, &mut members);
                members.push(Member {
                    name,
                    offset: byte,
                    size: None,
                });
                continue;
            };
            let label = format!("{name}:{width}");
            bits = match bits.take() {
                Some((s, e, names)) if start < e => {
                    Some((s, e.max(end), format!("{names}, {label}")))
                }
                previous => {
                    if let Some(previous) = previous {
                        flush_bits(&mut Some(previous), &mut members);
                    }
                    Some((start, end, label))
                }
            };
            continue;
        }
        flush_bits(&mut bits, &mut members);
        let Ok(offset) = offset.parse::<u64>() else {
            continue;
        };
        // `struct Empty e (empty)`: a `[[no_unique_address]]` or otherwise
        // empty member. Its sizeof still bounds it, and the next member
        // starting at the same offset shows it takes no space.
        let decl = decl.strip_suffix(" (empty)").unwrap_or(decl);
        if decl.ends_with("vtable pointer)")
            || decl.ends_with("vbtable pointer)")
            || decl.ends_with("vftable pointer)")
        {
            members.push(Member {
                name: "(vptr)".to_string(),
                offset,
                size: model.get("POINTER"),
            });
            continue;
        }
        if let Some(at) = decl
            .find(" (primary base)")
            .or_else(|| decl.find(" (base)"))
            .or_else(|| decl.find(" (virtual base)"))
        {
            let base = strip_tag(&decl[..at]);
            // A base occupies its data size (0 for an empty base); the derived
            // class may reuse its tail padding.
            let size = records.get(base).map(|(_, dsize)| *dsize);
            members.push(Member {
                name: format!("(base {base})"),
                offset,
                size,
            });
            continue;
        }
        let name = member_name(decl);
        let ty = decl_type(decl);
        members.push(Member {
            name,
            offset,
            size: type_size(ty, model, records),
        });
    }
    flush_bits(&mut bits, &mut members);
    members
}

fn flush_bits(bits: &mut Option<(u64, u64, String)>, members: &mut Vec<Member>) {
    if let Some((start, end, names)) = bits.take() {
        members.push(Member {
            name: names,
            offset: start,
            size: Some(end - start),
        });
    }
}

/// `struct Inner` -> `Inner`.
fn strip_tag(ty: &str) -> &str {
    let ty = ty.trim();
    ["struct ", "class ", "union ", "enum "]
        .iter()
        .find_map(|tag| ty.strip_prefix(tag))
        .unwrap_or(ty)
}

/// Is the declaration an unnamed member (`union X::(anonymous at f.h:3:5)`)?
fn is_anonymous(decl: &str) -> bool {
    decl.ends_with(')') && (decl.contains("(anonymous at ") || decl.contains("(unnamed at "))
}

fn member_name(decl: &str) -> String {
    if is_anonymous(decl) {
        return "(anonymous)".to_string();
    }
    decl.split_whitespace().last().unwrap_or("").to_string()
}

fn decl_type(decl: &str) -> &str {
    if is_anonymous(decl) {
        return decl;
    }
    match decl.rsplit_once(char::is_whitespace) {
        Some((ty, _name)) => ty.trim(),
        None => decl,
    }
}

/// Pull a numeric value out of `key=NN` inside a clang trailer line.
fn extract_kv(line: &str, key: &str) -> Option<u64> {
    let after = line.split(key).nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok()
}

/// The size of a member of (canonical) type `ty`, or `None` when it is not
/// provable — which makes the hole analysis honestly partial rather than wrong.
fn type_size(ty: &str, model: &DataModel, records: &HashMap<&str, (u64, u64)>) -> Option<u64> {
    let ty = ty.trim();
    // A pointer to member is 8 or 16 bytes depending on what it points to.
    if ty.contains("::*") {
        return None;
    }
    // Arrays, innermost extents last: `char[56]`, `int[2][3]`, `char *[4]`.
    if ty.ends_with(']') {
        let first = ty.find('[')?;
        let mut count = 1u64;
        for extent in ty[first..]
            .split(['[', ']'])
            .filter(|part| !part.is_empty())
        {
            count = count.checked_mul(extent.trim().parse().ok()?)?;
        }
        return type_size(&ty[..first], model, records)?.checked_mul(count);
    }
    if ty.ends_with('*') || ty.ends_with('&') || ty.contains("(*)") || ty.contains("(&)") {
        return model.get("POINTER");
    }
    let ty = ty.strip_prefix("const ").unwrap_or(ty);
    let ty = ty.strip_prefix("volatile ").unwrap_or(ty);
    if ty.starts_with("struct ") || ty.starts_with("class ") || ty.starts_with("union ") {
        return records.get(strip_tag(ty)).map(|(size, _)| *size);
    }
    let key = match ty {
        "bool" | "_Bool" | "char" | "signed char" | "unsigned char" | "char8_t" => return Some(1),
        "char16_t" => return Some(2),
        "char32_t" => return Some(4),
        "short" | "unsigned short" => "SHORT",
        "int" | "unsigned int" => "INT",
        "long" | "unsigned long" => "LONG",
        "long long" | "unsigned long long" => "LONG_LONG",
        "float" => "FLOAT",
        "double" => "DOUBLE",
        "long double" => "LONG_DOUBLE",
        "wchar_t" => "WCHAR_T",
        "__int128" | "unsigned __int128" => "INT128",
        "std::nullptr_t" | "decltype(nullptr)" => "POINTER",
        // Enums (their underlying type is not in the dump) and anything else.
        _ => return None,
    };
    model.get(key)
}

/// The target's scalar sizes, read from the compiler's own `__SIZEOF_*__`
/// macros under the same flags. A fixed table was LP64-only: `-m32` (long
/// and pointers 4, long double 12) and Windows targets (long 4, wchar_t 2)
/// got x86-64 Linux sizes, so offsets were exact and holes invented.
#[derive(Debug, Default, Clone, PartialEq)]
struct DataModel(HashMap<String, u64>);

impl DataModel {
    fn from_macros(text: &str) -> Self {
        let mut sizes = HashMap::new();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("#define") {
                continue;
            }
            let (Some(name), Some(value)) = (words.next(), words.next()) else {
                continue;
            };
            if let Some(key) = name
                .strip_prefix("__SIZEOF_")
                .and_then(|k| k.strip_suffix("__"))
            {
                if let Ok(size) = value.parse() {
                    sizes.insert(key.to_string(), size);
                }
            }
        }
        Self(sizes)
    }

    fn get(&self, key: &str) -> Option<u64> {
        self.0.get(key).copied()
    }
}

// ----- pahole parsing ----------------------------------------------------

/// Parse `pahole` output. Each data member line carries an exact
/// `/* <offset> <size> */` comment, and the summary a `/* size: N ... */`.
fn parse_pahole(stdout: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let mut current: Option<Record> = None;
    // Nesting inside the current record. An inline struct/union member's own
    // members are NOT the record's members: the member is reported once, on
    // its closing `} name; /* off size */` line. pahole also prints an
    // expansion of each anonymous nested type (offsets from 0, a bare `};`),
    // which is not a member at all. Treating a nested `union {` as a new
    // record — as this parser once did — threw away the outer record and
    // reported nameless "size 0" records instead.
    let mut depth = 0usize;
    for line in stdout.lines() {
        let trimmed = line.trim();
        let Some(rec) = current.as_mut() else {
            // A record header starts at column 0: `struct Foo {`.
            if !line.starts_with(char::is_whitespace) && trimmed.ends_with('{') {
                // `typedef struct {` names its record on the closing line.
                let header = trimmed.strip_prefix("typedef ").unwrap_or(trimmed);
                if let Some(name) = header
                    .strip_prefix("struct ")
                    .or_else(|| header.strip_prefix("class "))
                    .or_else(|| header.strip_prefix("union "))
                {
                    // `class Derived : public Base {` is named `Derived`.
                    let name = name.trim_end_matches('{').split(" : ").next().unwrap_or("");
                    current = Some(Record {
                        name: name.trim().to_string(),
                        size: 0,
                        align: None,
                        members: Vec::new(),
                        source: "pahole (exact)",
                    });
                    depth = 0;
                }
            }
            continue;
        };
        if trimmed.starts_with('}') {
            if depth == 0 {
                if let Some(mut done) = current.take() {
                    if done.name.is_empty() {
                        // `} CTypedef;`
                        done.name = trimmed
                            .trim_start_matches('}')
                            .split(';')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                    }
                    records.push(done);
                }
                continue;
            }
            depth -= 1;
            if depth == 0 {
                // `} u; /* 16 4 */` or anonymous `}; /* 24 8 */`; a bare `};`
                // (no trailer) closes a type expansion and is skipped.
                if let Some((offset, Some(size))) = pahole_member_comment(trimmed) {
                    let name = trimmed
                        .trim_start_matches('}')
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim();
                    rec.members.push(Member {
                        name: if name.is_empty() {
                            "(anonymous)".to_string()
                        } else {
                            name.to_string()
                        },
                        offset,
                        size: Some(size),
                    });
                }
            }
            continue;
        }
        if trimmed.ends_with('{') {
            depth += 1;
            continue;
        }
        if depth > 0 {
            continue;
        }
        if trimmed.starts_with("/* size:") {
            if let Some(sz) = pahole_summary_size(trimmed) {
                rec.size = sz;
            }
            continue;
        }
        // A data member: "<type> <name>; /* <offset> <size> */". The ';' guard
        // keeps pahole summary comments ("/* sum members: 20 ... */") out.
        if !trimmed.contains(';') {
            continue;
        }
        let (Some((offset, size)), Some(name)) =
            (pahole_member_comment(trimmed), pahole_member_name(trimmed))
        else {
            continue;
        };
        match size {
            Some(size) => rec.members.push(Member {
                name,
                offset,
                size: Some(size),
            }),
            // A bitfield reports its storage unit: `/* 0: 3  4 */`. Fields
            // packed into the previous field's unit (same offset, or one
            // pahole marks "combined with next fields", e.g. `/* 1: 0 1 */`
            // inside a 4-byte unit at 0) become ONE member spanning the unit —
            // exact at byte granularity.
            None => {
                let unit = pahole_bitfield_unit(trimmed).unwrap_or(0);
                match rec.members.last_mut() {
                    Some(last)
                        if last.name.contains(':')
                            && offset >= last.offset
                            && offset < last.offset + last.size.unwrap_or(0) =>
                    {
                        last.name.push_str(&format!(", {name}"));
                        let end = (offset + unit).max(last.offset + last.size.unwrap_or(0));
                        last.size = Some(end - last.offset);
                    }
                    _ => rec.members.push(Member {
                        name,
                        offset,
                        size: Some(unit),
                    }),
                }
            }
        }
    }
    if let Some(done) = current.take() {
        records.push(done);
    }
    records
}

/// Parse the `/* <offset> <size> */` trailer of a pahole member line. A
/// bitfield's trailer is `/* <offset>: <bit> <unit> */`: its offset comes back
/// with `None` for the size (see [`pahole_bitfield_unit`]).
fn pahole_member_comment(line: &str) -> Option<(u64, Option<u64>)> {
    let comment = line.rsplit("/*").next()?;
    let comment = comment.split("*/").next()?;
    let tokens: Vec<&str> = comment.split_whitespace().collect();
    let first = tokens.first()?;
    if let Some(offset) = first.strip_suffix(':') {
        return Some((offset.parse().ok()?, None));
    }
    Some((first.parse().ok()?, Some(tokens.get(1)?.parse().ok()?)))
}

/// The storage-unit size of a bitfield trailer `/* <offset>: <bit> <unit> */`.
fn pahole_bitfield_unit(line: &str) -> Option<u64> {
    let comment = line.rsplit("/*").next()?.split("*/").next()?;
    comment.split_whitespace().nth(2)?.parse().ok()
}

/// The member name in a pahole member line (`<type> <declarator>;`): the
/// identifier of the declarator, with array extents (`tag[6]`), a bitfield
/// width (`a:3`, kept as the name) and function-pointer syntax (`(*cb)(int)`,
/// `(**_vptr.Foo)(void)`) understood. A base class line (`class Base
/// <ancestor>;`) is named `(base Base)`. Array members used to vanish here,
/// counting their bytes as padding.
fn pahole_member_name(line: &str) -> Option<String> {
    let decl = line.split(';').next()?.trim();
    // pahole 1.24 prints a base class commented out:
    // `/* struct Base <ancestor>; */ /* 0 16 */`.
    let decl = decl.strip_prefix("/*").map_or(decl, str::trim);
    if let Some(base) = decl.strip_suffix("<ancestor>") {
        let base = base.trim();
        let base = base
            .strip_prefix("class ")
            .or_else(|| base.strip_prefix("struct "))
            .unwrap_or(base);
        return Some(format!("(base {})", base.trim()));
    }
    if let Some(at) = decl.find("(*") {
        let name = decl[at..]
            .trim_start_matches(['(', '*'])
            .split(')')
            .next()?
            .trim();
        return (!name.is_empty()).then(|| name.to_string());
    }
    let (decl, width) = match decl.rsplit_once(':') {
        Some((before, width))
            if !width.trim().is_empty() && width.trim().chars().all(|c| c.is_ascii_digit()) =>
        {
            (before, Some(width.trim()))
        }
        _ => (decl, None),
    };
    let decl = decl.split('[').next().unwrap_or(decl).trim_end();
    let ident: String = decl
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if ident.is_empty() {
        return None;
    }
    Some(match width {
        Some(width) => format!("{ident}:{width}"),
        None => ident,
    })
}

fn pahole_summary_size(line: &str) -> Option<u64> {
    let after = line.split("size:").nth(1)?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<u64>().ok()
}

// ----- rendering ---------------------------------------------------------

fn render_records(records: &[Record], cacheline: u64) -> String {
    records
        .iter()
        .map(|r| render_record(r, cacheline))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_record(rec: &Record, cacheline: u64) -> String {
    let mut out = match rec.align {
        Some(a) => format!(
            "{}: size {}, align {}  (source: {})\n",
            rec.name, rec.size, a, rec.source
        ),
        None => format!(
            "{}: size {}  (source: {})\n",
            rec.name, rec.size, rec.source
        ),
    };

    let mut all_known = true;
    let mut member_bytes = 0u64;
    let mut straddles = Vec::new();

    for (i, m) in rec.members.iter().enumerate() {
        let size_str = m
            .size
            .map(|s| format!("{s} B"))
            .unwrap_or_else(|| "? B".to_string());
        out.push_str(&format!(
            "  {off:>4}  {name}  ({size_str})\n",
            off = m.offset,
            name = m.name
        ));

        match m.size {
            Some(sz) => {
                // A member that the next one starts inside (a derived class
                // reusing its base's tail padding, `[[no_unique_address]]`)
                // only owns the bytes up to it; counting its full size twice
                // reported "0 bytes of padding" for a Derived with 3.
                member_bytes += match rec.members.get(i + 1) {
                    Some(next) if next.offset >= m.offset && next.offset < m.offset + sz => {
                        next.offset - m.offset
                    }
                    _ => sz,
                };
                if sz > 0 {
                    let start = m.offset / cacheline;
                    let end = (m.offset + sz - 1) / cacheline;
                    if end > start {
                        straddles.push((m.name.clone(), start, end, m.offset, m.offset + sz));
                    }
                }
                if let Some(next) = rec.members.get(i + 1) {
                    let this_end = m.offset + sz;
                    if next.offset > this_end {
                        out.push_str(&format!(
                            "        -- {}-byte hole --\n",
                            next.offset - this_end
                        ));
                    }
                }
            }
            None => {
                // A member whose size this backend cannot prove (a nested /
                // opaque type like std::string) means the GAP after it is
                // unknown — it is NOT a confirmed hole. Never print the word
                // "hole" here; that mislabels ground truth (e.g. a struct with
                // std::string members that actually has no padding).
                all_known = false;
                out.push_str(&format!(
                    "        -- gap after '{}' not computed: its type's size isn't provable from \
                     this backend (nested/opaque type) — NOT necessarily padding --\n",
                    m.name
                ));
            }
        }
    }

    // Tail padding (only when the last member's size is provable).
    let tail_known = match rec.members.last() {
        Some(last) => match last.size {
            Some(sz) => {
                let end = last.offset + sz;
                if rec.size > end {
                    out.push_str(&format!(
                        "        -- {} bytes tail padding --\n",
                        rec.size - end
                    ));
                }
                true
            }
            None => false,
        },
        None => true,
    };

    // Summary line.
    if all_known && tail_known {
        let padding = rec.size.saturating_sub(member_bytes);
        let pct = if rec.size > 0 {
            padding as f64 / rec.size as f64 * 100.0
        } else {
            0.0
        };
        out.push_str(&format!(
            "{padding} of {} bytes are padding ({pct:.0}%).",
            rec.size
        ));
    } else {
        out.push_str(
            "hole analysis: partial — some members have types whose size this backend cannot \
             prove, so gaps around them are UNKNOWN (not necessarily holes, and not necessarily \
             absent). Do NOT conclude the record has (or lacks) padding from this; build with -g \
             and use pahole (Linux) for the exact hole map.",
        );
    }

    // Cacheline straddles.
    if straddles.is_empty() {
        if all_known && tail_known {
            out.push_str(&format!(" No member straddles a {cacheline}B cacheline."));
        }
    } else {
        for (name, a, b, from, to) in straddles {
            out.push_str(&format!(
                "\nmember '{name}' spans cachelines {a}-{b} (offset {from}..{to}) — hot-path \
                 access pays two lines."
            ));
        }
    }
    out
}

// ----- compile_commands.json flag extraction -----------------------------

/// Best-effort: pull the flags that decide how `file` compiles — and so how its
/// records are laid out — from the project's compile_commands.json, so a real
/// project's headers resolve. Returns an empty vec when there is no DB or no
/// matching entry.
///
/// Shares the database locator, the shell-quote-aware `command` splitter and
/// the target-flag allowlist with `cpp_sanitize`. This used to split `command`
/// on whitespace, never resolve a relative path against the entry's
/// `directory`, and match entries by bare file name, so a CMake string define
/// (`-DVERSION=\"1.0\"`), a quoted include dir with a space, an out-of-source
/// `-I../include` and a separate `-I dir` each made the compile fail, and
/// `-m32` was dropped (a 32-bit build's pointers are 4 bytes).
fn compile_db_flags(cwd: &Path, file: &str) -> Vec<String> {
    use super::sanitizers::canonical;
    use super::sanitizers::resolve_path;

    let Some(db) = super::compile_db::find_compile_db(cwd) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&db) else {
        return Vec::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let want = canonical(&resolve_path(cwd, file));
    let entry_path = |entry: &Value| {
        let directory = entry.get("directory").and_then(Value::as_str).unwrap_or("");
        let efile = entry.get("file").and_then(Value::as_str)?;
        Some(resolve_path(Path::new(directory), efile))
    };
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry_path(entry).is_some_and(|path| canonical(&path) == want))
    {
        return extract_entry_flags(entry);
    }
    // A database generated elsewhere (a container, another checkout) cannot
    // match by path; fall back to the file name, but only when exactly one
    // entry has it — another file's flags are worse than none.
    let name = Path::new(file).file_name();
    let mut same_name = entries
        .iter()
        .filter(|entry| entry_path(entry).is_some_and(|path| path.file_name() == name));
    match (same_name.next(), same_name.next()) {
        (Some(entry), None) => extract_entry_flags(entry),
        _ => Vec::new(),
    }
}

/// Keep only the header/macro/standard/target-ABI flags from a compile-DB
/// entry, resolving relative paths against its `directory`; drop
/// output/compile-action flags that would break the layout compile.
fn extract_entry_flags(entry: &Value) -> Vec<String> {
    use super::sanitizers::is_safe_target_flag;
    use super::sanitizers::resolve_path;
    use super::sanitizers::shell_split;

    let directory = Path::new(entry.get("directory").and_then(Value::as_str).unwrap_or(""));
    let tokens: Vec<String> = if let Some(args) = entry.get("arguments").and_then(Value::as_array) {
        args.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    } else if let Some(cmd) = entry.get("command").and_then(Value::as_str) {
        shell_split(cmd)
    } else {
        return Vec::new();
    };
    let path = |value: &str| resolve_path(directory, value).display().to_string();
    const PATH_FLAGS: [&str; 5] = ["-I", "-isystem", "-iquote", "-idirafter", "-include"];
    let mut flags = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if PATH_FLAGS.contains(&t) || t == "-D" || t == "-U" {
            // Separate-value form: `-I dir`, `-D NAME`.
            if let Some(value) = tokens.get(i + 1) {
                flags.push(t.to_string());
                flags.push(if t == "-D" || t == "-U" {
                    value.clone()
                } else {
                    path(value)
                });
                i += 1;
            }
        } else if let Some(dir) = ["-isystem", "-iquote", "-idirafter", "-I"]
            .iter()
            .find_map(|flag| t.strip_prefix(flag).map(|dir| (*flag, dir)))
        {
            flags.push(format!("{}{}", dir.0, path(dir.1)));
        } else if t.starts_with("-D") || t.starts_with("-U") || t.starts_with("-std=") {
            flags.push(t.to_string());
        } else if is_safe_target_flag(t) {
            flags.push(t.to_string());
        }
        i += 1;
    }
    flags
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
