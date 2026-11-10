use std::collections::BTreeMap;
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

use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;

const TOOL_NAME: &str = "cpp_binary";
const DEFAULT_ACTION: &str = "headers";

/// Output budget. These tools can dump megabytes (a full `objdump -d` of a real
/// binary), so every result is capped before it reaches the model.
const MAX_LINES: usize = 200;
const MAX_BYTES: usize = 12 * 1024;

pub struct BinaryTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinaryArgs {
    binary: String,
    #[serde(default = "default_action")]
    action: String,
    #[serde(default)]
    symbol: Option<String>,
}

fn default_action() -> String {
    DEFAULT_ACTION.to_string()
}

/// The inspection modes this tool exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Disasm,
    Symbols,
    Headers,
    Dynamic,
    Abi,
}

impl Action {
    fn parse(s: &str) -> Result<Action, String> {
        match s {
            "disasm" => Ok(Action::Disasm),
            "symbols" => Ok(Action::Symbols),
            "headers" => Ok(Action::Headers),
            "dynamic" => Ok(Action::Dynamic),
            "abi" => Ok(Action::Abi),
            other => Err(format!(
                "unknown action {other:?}; expected one of: disasm, symbols, headers, dynamic, abi"
            )),
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for BinaryTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "binary".to_string(),
            JsonSchema::string(Some(
                "Path to the compiled artifact to inspect — an ELF executable, object (.o), \
                 static archive (.a), or shared object (.so) — relative to the workspace or \
                 absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    serde_json::json!("disasm"),
                    serde_json::json!("symbols"),
                    serde_json::json!("headers"),
                    serde_json::json!("dynamic"),
                    serde_json::json!("abi"),
                ],
                Some(
                    "What to inspect (default \"headers\"). \"abi\": diagnose a link failure — \
                     pass the symbol from the \"undefined reference\" error as `symbol` and it \
                     reports whether this artifact DEFINES it, merely references it (U), or does \
                     not have it at all, and detects the libstdc++ dual-ABI mismatch \
                     (std::__cxx11::basic_string vs std::string) that makes a correct-looking \
                     symbol un-linkable; with no `symbol` it summarises the artifact's ABI \
                     posture so you can compare two artifacts. \"headers\": ELF header + program \
                     headers + section headers — class/type (PIE), machine/entry, sections, and \
                     the hardening segments GNU_STACK (NX) and GNU_RELRO. \"dynamic\": dynamic \
                     section — NEEDED libraries, SONAME, RPATH/RUNPATH, BIND_NOW (full RELRO). \
                     \"symbols\": demangled symbol table, also how you check for a stack canary \
                     (__stack_chk_fail) (falls back to dynamic symbols when stripped). \"disasm\": \
                     Intel-syntax disassembly — pass `symbol` to scope it to one function."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "symbol".to_string(),
            JsonSchema::string(Some(
                "For action=\"disasm\": disassemble only this (demangled) function name instead \
                 of the whole binary. Strongly recommended — a full disassembly is truncated. \
                 For action=\"abi\": the symbol from the linker's \"undefined reference\" error, \
                 copied verbatim — demangled (metrics::record(std::string const&, double)) or \
                 mangled (_ZN7metrics6recordERKSsd) both work, and a bare name \
                 (metrics::record) matches every overload. For action=\"symbols\": list only \
                 the symbols whose name contains this text."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Diagnose linker/ABI errors and inspect compiled binaries/objects/\
                libraries with objdump/nm/readelf. Reach for it on an 'undefined reference' or an \
                ABI mismatch (e.g. std::__cxx11::basic_string vs std::string) — `nm -C` the archive \
                to see which symbol is missing or differently mangled. Also: ELF headers & sections, \
                dynamic deps/RPATH/RUNPATH, symbol tables, security hardening (PIE/RELRO/NX/stack \
                canary), or disassembly of a function. Use to triage link failures, check what a \
                binary links against, inspect stripped/third-party binaries, or verify codegen. \
                C/C++ binaries only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["binary".to_string()]),
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

impl BinaryTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: BinaryArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let action = Action::parse(&args.action).map_err(FunctionCallError::RespondToModel)?;

        let binary = resolve_path(&self.cwd, &args.binary);
        if !binary.exists() {
            return Err(FunctionCallError::RespondToModel(format!(
                "binary not found: {}",
                binary.display()
            )));
        }
        let binary_str = binary.to_string_lossy().to_string();

        // Empty/whitespace-only symbol is treated as "no symbol".
        let symbol = args
            .symbol
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let body = match action {
            Action::Abi => self.abi(&binary_str, symbol).await?,
            Action::Headers => {
                let out = run_tool("readelf", &headers_args(&binary_str), &self.cwd).await?;
                budget(&combine_output(&out.stdout, &out.stderr))
            }
            Action::Dynamic => {
                let out = run_tool("readelf", &dynamic_args(&binary_str), &self.cwd).await?;
                budget(&combine_output(&out.stdout, &out.stderr))
            }
            Action::Symbols => {
                let all = run_tool("nm", &symbols_args(&binary_str, false), &self.cwd).await?;
                let (table, text) = if no_symbols(&all.stdout) {
                    // Stripped: retry against the dynamic symbol table.
                    let dynamic = run_tool("nm", &symbols_args(&binary_str, true), &self.cwd).await?;
                    if no_symbols(&dynamic.stdout) {
                        // nm's own error ("file format not recognized",
                        // "Is a directory") is the answer — it must not be
                        // reported as "binary is stripped".
                        let why = String::from_utf8_lossy(&all.stderr).trim().to_string();
                        return Ok(crate::report::report(
                            TOOL_NAME,
                            &args.binary,
                            no_symbols_message(&why),
                            check_result::CheckResult::no_claim(TOOL_NAME, "No symbol table could be read from this file."),
                        ));
                    }
                    ("dynamic symbols (nm -D; the static table is empty — stripped)", dynamic)
                } else {
                    ("symbols (nm; `U` = undefined, imported from a library)", all)
                };
                let listing = combine_output(&text.stdout, &text.stderr);
                match symbol {
                    // Filter BEFORE the line budget — std::thread::join
                    // is line 3843 of libstdc++'s table and would read as absent.
                    Some(name) => {
                        let hits: Vec<&str> =
                            listing.lines().filter(|line| line.contains(name)).collect();
                        if hits.is_empty() {
                            format!("{table}: no symbol containing `{name}`.")
                        } else {
                            budget(&format!(
                                "{table}, {} matching `{name}`:\n{}",
                                hits.len(),
                                hits.join("\n")
                            ))
                        }
                    }
                    None => budget(&format!("{table}:\n{listing}")),
                }
            }
            Action::Disasm => {
                let out = run_tool("objdump", &disasm_args(&binary_str, symbol), &self.cwd).await?;
                let text = combine_output(&out.stdout, &out.stderr);
                let budgeted = budget(&text);
                if symbol.is_none() {
                    format!(
                        "{budgeted}\n(Full-binary disassembly is capped at {MAX_LINES} lines. \
                         Pass `symbol` to disassemble just one function.)"
                    )
                } else {
                    budgeted
                }
            }
        };

        // `budget` marks a cut listing; a cut listing is not the whole answer.
        let result = if body.contains("… [truncated: showing ") {
            check_result::CheckResult::approximate(
                TOOL_NAME,
                "The listing was longer than this tool shows, so it was cut (pass `symbol` to narrow it).",
            )
        } else {
            check_result::CheckResult::pass(TOOL_NAME, "Read the binary with the system's binutils.")
        };
        Ok(crate::report::report(TOOL_NAME, &args.binary, body, result))
    }

    /// `abi`: answer "why won't this link?".
    ///
    /// Reads the symbol table twice — raw and demangled — because the two
    /// spellings answer different halves of the question: the demangled view is
    /// what the user's linker error looks like, the mangled view is what the
    /// linker actually matched on, and the dual-ABI difference is only
    /// *unambiguous* in the mangling.
    async fn abi(&self, binary: &str, symbol: Option<&str>) -> Result<String, FunctionCallError> {
        let mut raw = run_tool("nm", &abi_args(binary, false, false), &self.cwd).await?;
        let mut demangled = run_tool("nm", &abi_args(binary, true, false), &self.cwd).await?;
        let mut dynamic = false;
        if no_symbols(&raw.stdout) && no_symbols(&demangled.stdout) {
            // Stripped shared object: the dynamic table is all that is left, and
            // for a .so it is also the table the linker consults.
            let raw_dyn = run_tool("nm", &abi_args(binary, false, true), &self.cwd).await?;
            let dem_dyn = run_tool("nm", &abi_args(binary, true, true), &self.cwd).await?;
            if !no_symbols(&raw_dyn.stdout) || !no_symbols(&dem_dyn.stdout) {
                raw = raw_dyn;
                demangled = dem_dyn;
                dynamic = true;
            }
        }

        let mut body = abi_body(
            symbol,
            &String::from_utf8_lossy(&raw.stdout),
            &String::from_utf8_lossy(&demangled.stdout),
            dynamic,
        );
        // With no symbols at all, nm's own complaint ("File format not
        // recognized") is the answer — otherwise we would report the tool's
        // failure as a fact about the artifact.
        if no_symbols(&raw.stdout) && no_symbols(&demangled.stdout) {
            let note = combine_output(b"", &raw.stderr);
            if !note.is_empty() {
                body.push('\n');
                body.push_str(&note);
                body.push('\n');
            }
        }
        Ok(budget(&body))
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// `readelf -h -l -S <bin>`: ELF header (class/type/machine/entry) + program
/// headers + section headers.
///
/// `-l` is what makes the security-hardening question answerable: GNU_STACK
/// (NX) and GNU_RELRO live in the program headers, so without it the tool could
/// not show the very thing its description promises.
fn headers_args(binary: &str) -> Vec<String> {
    vec![
        "-h".to_string(),
        "-l".to_string(),
        "-S".to_string(),
        binary.to_string(),
    ]
}

/// `readelf -d <bin>`: dynamic section (NEEDED, SONAME, RPATH/RUNPATH).
fn dynamic_args(binary: &str) -> Vec<String> {
    vec!["-d".to_string(), binary.to_string()]
}

/// `nm` argv. `dynamic` selects the dynamic-symbol fallback (`-D`) used when the
/// regular symbol table is empty because the binary is stripped.
///
/// NOT `--defined-only`. The stack-canary check the description promises
/// is the presence of `U __stack_chk_fail` — an undefined import — so that
/// flag would make a `-fstack-protector-all` binary read as "no canary".
fn symbols_args(binary: &str, dynamic: bool) -> Vec<String> {
    if dynamic {
        vec!["-D".to_string(), "-C".to_string(), binary.to_string()]
    } else {
        vec!["-C".to_string(), binary.to_string()]
    }
}

/// Neither symbol table had anything. Stripped, or not something nm can read
/// at all — and then nm's own complaint is the answer.
fn no_symbols_message(nm_stderr: &str) -> String {
    let unreadable = ["format not recognized", "Is a directory", "No such file", "not recognized"]
        .iter()
        .any(|needle| nm_stderr.contains(needle));
    if unreadable {
        format!("nm could not read this file as a binary: {nm_stderr}")
    } else {
        "no symbols (binary is stripped, no dynamic symbols either).".to_string()
    }
}

/// `nm` argv for the `abi` action. `demangle` adds `-C`; `dynamic` switches to
/// the dynamic symbol table (`-D`) for a stripped shared object.
///
/// Deliberately WITHOUT `--defined-only` (like [`symbols_args`]): the
/// undefined (`U`) entries ARE the evidence for a link failure, so filtering
/// them out would remove the answer.
fn abi_args(binary: &str, demangle: bool, dynamic: bool) -> Vec<String> {
    let mut argv = Vec::new();
    if dynamic {
        argv.push("-D".to_string());
    }
    if demangle {
        argv.push("-C".to_string());
    }
    argv.push(binary.to_string());
    argv
}

// ---------------------------------------------------------------------------
// ABI / link diagnosis
// ---------------------------------------------------------------------------

/// Max matched symbols enumerated in one diagnosis. [`budget`] still caps the
/// whole body; this keeps the *diagnosis* readable when a bare name matches
/// dozens of overloads.
const ABI_MATCH_CAP: usize = 10;

/// Stand-in for every spelling of `std::string` during normalization. Chosen so
/// it can occur in no real mangled or demangled name (and contains no `Ss`, so
/// [`replace_legacy_abbrev`] cannot chew on its own output).
const ABI_STRING_PLACEHOLDER: &str = "@STRING@";

/// One parsed `nm` line: the type letter plus the name exactly as printed
/// (demangled or mangled, depending on which invocation produced it).
#[derive(Debug, Clone, PartialEq, Eq)]
struct NmSymbol {
    kind: char,
    name: String,
}

impl NmSymbol {
    /// `U` is an undefined reference. Lowercase `v`/`w` are *weak* undefined —
    /// they link without error but are not definitions, so a link that expected
    /// real code still misbehaves.
    fn is_undefined(&self) -> bool {
        matches!(self.kind, 'U' | 'v' | 'w')
    }

    fn is_defined(&self) -> bool {
        !self.is_undefined()
    }
}

/// Which side of the libstdc++ dual ABI a symbol's spelling implies.
///
/// GCC 5 introduced a second, incompatible `std::string`
/// (`std::__cxx11::basic_string`, mangled `NSt7__cxx1112basic_string…`) selected
/// by `_GLIBCXX_USE_CXX11_ABI=1`, which is the default. Code built with `=0`
/// keeps the pre-GCC-5 `std::string`, abbreviated `Ss` in the mangling. The two
/// mangle differently, so a library built one way and a caller built the other
/// way produce "undefined reference" even though the declarations match
/// character for character — which is exactly why this is worth detecting
/// rather than leaving the model to eyeball a symbol dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbiFlavour {
    /// `_GLIBCXX_USE_CXX11_ABI=1` — `std::__cxx11::basic_string`.
    Cxx11,
    /// `_GLIBCXX_USE_CXX11_ABI=0` — the pre-GCC-5 `std::string` (`Ss`).
    Legacy,
    /// No `std::string` in the signature; the dual ABI does not apply.
    Neutral,
}

impl AbiFlavour {
    fn label(self) -> &'static str {
        match self {
            AbiFlavour::Cxx11 => "new ABI (std::__cxx11::basic_string, _GLIBCXX_USE_CXX11_ABI=1)",
            AbiFlavour::Legacy => "old ABI (std::string, _GLIBCXX_USE_CXX11_ABI=0)",
            AbiFlavour::Neutral => "no std::string in the signature, so the dual ABI is not involved",
        }
    }

    /// The flag a *consumer* must be compiled with to match this flavour.
    fn consumer_flag(self) -> &'static str {
        match self {
            AbiFlavour::Cxx11 => "-D_GLIBCXX_USE_CXX11_ABI=1",
            AbiFlavour::Legacy => "-D_GLIBCXX_USE_CXX11_ABI=0",
            AbiFlavour::Neutral => "",
        }
    }
}

/// Parse `nm` output into symbols.
///
/// Handles the ordinary form (`0000000000000000 T main`), the undefined form
/// (the address column is blank: `                 U printf`), and archives,
/// whose members are introduced by a `member.o:` header line carrying no symbol.
fn parse_nm(text: &str) -> Vec<NmSymbol> {
    text.lines().filter_map(parse_nm_line).collect()
}

fn parse_nm_line(line: &str) -> Option<NmSymbol> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.ends_with(':') {
        // Blank separator, or an archive member header ("metrics.o:").
        return None;
    }
    let first = trimmed.split_whitespace().next()?;
    // After the (possibly blank) address column, a symbol line is either
    // "<addr> <kind> <name>" or, for an undefined symbol, "<kind> <name>".
    let after_addr = if first.len() == 1 {
        trimmed
    } else if first.chars().all(|c| c.is_ascii_hexdigit()) {
        trimmed[first.len()..].trim_start()
    } else {
        return None;
    };
    let mut chars = after_addr.chars();
    let kind = chars.next()?;
    let rest = chars.as_str();
    // The type is one letter followed by whitespace; anything else is not a
    // symbol line (nm interleaves diagnostics with its output).
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let name = rest.trim_start();
    if name.is_empty() {
        return None;
    }
    Some(NmSymbol {
        kind,
        name: name.to_string(),
    })
}

/// Classify one symbol name. Works on both the mangled and demangled spelling,
/// because the `abi` action reads both.
fn abi_flavour(name: &str) -> AbiFlavour {
    if name.contains("__cxx11") {
        AbiFlavour::Cxx11
    } else if mentions_legacy_string(name) {
        AbiFlavour::Legacy
    } else {
        AbiFlavour::Neutral
    }
}

/// Old-ABI `std::string`, in either spelling. Callers check `__cxx11` first, so
/// `std::basic_string<char` here can only be the old one.
fn mentions_legacy_string(name: &str) -> bool {
    if name.contains("std::string") || name.contains("std::basic_string<char") {
        return true;
    }
    // The `Ss` abbreviation only means std::string inside an Itanium mangled
    // name. In a demangled one it is just two letters (`crypto::Ssl_init()`),
    // and reading it as a string type would invent an ABI mismatch.
    is_mangled(name) && replace_legacy_abbrev(name) != name
}

/// Itanium C++ mangled names all begin `_Z`. Used to scope the `Ss`
/// abbreviation rule to the spelling where it is defined.
fn is_mangled(name: &str) -> bool {
    name.starts_with("_Z")
}

/// Replace the Itanium `Ss` abbreviation for the old-ABI `std::string` — but
/// never an `Ss` inside an identifier. Identifiers in a mangled name are
/// length-prefixed (`8checkSsl`), so a digit run followed by a name is skipped
/// whole: `_Z8checkSsli` (`checkSsl(int)`) would otherwise read as old-ABI because
/// its `Ss` follows a `k`, not a digit. A digit run followed by `_` or `E` is a
/// substitution index, array bound or literal, not a length, and is copied.
fn replace_legacy_abbrev(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &name[start..i];
            let is_length = !matches!(bytes.get(i), None | Some(b'_') | Some(b'E'));
            let len: usize = digits.parse().unwrap_or(usize::MAX);
            out.push_str(digits);
            if is_length && i.checked_add(len).is_some_and(|end| end <= bytes.len()) {
                if let Some(ident) = name.get(i..i + len) {
                    out.push_str(ident);
                    i += len;
                }
            }
            continue;
        }
        if bytes[i] == b'S' && bytes.get(i + 1) == Some(&b's') {
            out.push_str(ABI_STRING_PLACEHOLDER);
            i += 2;
            continue;
        }
        // Advance by whole chars so a non-ASCII name survives intact.
        let ch = name[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Normalize a symbol name so the two dual-ABI spellings of the SAME function
/// compare equal. Whitespace is dropped (demangled output varies between `> >`
/// and `>>`) and every spelling of `std::string` collapses to one placeholder.
fn abi_base(name: &str) -> String {
    let mut n = name.replace(' ', "");
    for needle in [
        // Longest first: the shorter mangled needle is a substring of the
        // longer one (`N` + inner + `E`), so order decides correctness.
        "std::__cxx11::basic_string<char,std::char_traits<char>,std::allocator<char>>",
        "std::basic_string<char,std::char_traits<char>,std::allocator<char>>",
        "std::__cxx11::string",
        "std::string",
        "NSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEE",
        "St7__cxx1112basic_stringIcSt11char_traitsIcESaIcEE",
    ] {
        n = n.replace(needle, ABI_STRING_PLACEHOLDER);
    }
    if is_mangled(&n) {
        replace_legacy_abbrev(&n)
    } else {
        n
    }
}

/// Does `sym` name the entity the user asked about?
///
/// This used to be a SUBSTRING test for every query, which invented
/// answers: `record` matched `recorder(double)` and read DEFINED,
/// `_Z6recordd` matched `_Z6recorddi`, `record(double)` matched
/// `metrics::record(double)`, and `log(std::string const&)` matched
/// `metrics::log` and reported a dual-ABI mismatch that did not exist. Now a
/// signature or a mangled name must match EXACTLY (after ABI normalization);
/// only a bare name finds every overload, and only on a name boundary: the
/// symbol's name without its parameter list must be the query, or end in
/// `::` + the query.
fn base_matches(sym: &str, query_base: &str) -> bool {
    // A dynamic symbol carries its version (`__stack_chk_fail@GLIBC_2.4`,
    // `…@@GLIBCXX_3.4`); it is the same symbol as the unversioned spelling.
    // Stripped from the RAW name: the normalized one contains the `@STRING@`
    // placeholder.
    let base = abi_base(without_symbol_version(sym));
    let base = base.as_str();
    if base == query_base {
        return true;
    }
    if is_mangled(query_base) || query_base.contains('(') {
        return false;
    }
    let name = base.split('(').next().unwrap_or(base);
    name == query_base || name.ends_with(&format!("::{query_base}"))
}

/// `name@VERSION` / `name@@VERSION` -> `name`. Only a trailing version tag
/// (no parentheses or spaces after the `@`) is removed.
fn without_symbol_version(name: &str) -> &str {
    match name.find('@') {
        Some(at) if !name[at..].contains(['(', ')', ' ']) => &name[..at],
        _ => name,
    }
}

/// Render the `abi` diagnosis from the two `nm` views of one artifact.
///
/// Pure, and split out from the handler on purpose: it is tested against real
/// `nm` output captured from known-ABI objects, with no binutils on the test
/// host and no artifact on disk.
fn abi_body(query: Option<&str>, raw_text: &str, demangled_text: &str, dynamic: bool) -> String {
    let raw = parse_nm(raw_text);
    let demangled = parse_nm(demangled_text);

    let table = if dynamic {
        "dynamic symbol table (nm -D — the static table was empty, so this artifact is stripped)"
    } else {
        "symbol table (nm)"
    };
    let mut out = format!("Link/ABI diagnosis, read from the {table}.\n");

    if let Some(query) = query {
        out.push_str(&render_query(query, &raw, &demangled));
    }
    out.push_str(&render_posture(&raw, &demangled));
    if query.is_none() {
        out.push_str(
            "\nNo `symbol` given, so this is a posture summary only. To diagnose one \"undefined \
             reference\", call this action again with the symbol from the linker error as \
             `symbol`. To compare two artifacts, run it on each and compare the posture lines.\n",
        );
    }
    out
}

fn render_query(query: &str, raw: &[NmSymbol], demangled: &[NmSymbol]) -> String {
    let query = query.trim();
    let query_flavour = abi_flavour(query);
    let query_base = abi_base(query);

    // Same-flavour matches answer "is it here?"; cross-flavour matches are the
    // dual-ABI diagnosis. A Neutral query (no std::string) can never be a
    // mismatch, so everything it matches counts as same-flavour.
    let mut same: Vec<&NmSymbol> = Vec::new();
    let mut cross: Vec<&NmSymbol> = Vec::new();
    for sym in raw.iter().chain(demangled.iter()) {
        if !base_matches(&sym.name, &query_base) {
            continue;
        }
        let flavour = abi_flavour(&sym.name);
        if query_flavour != AbiFlavour::Neutral
            && flavour != AbiFlavour::Neutral
            && flavour != query_flavour
        {
            cross.push(sym);
        } else {
            same.push(sym);
        }
    }
    let defined_same = same.iter().any(|s| s.is_defined());

    let mut out = format!(
        "\nSymbol asked about: {query}\n  ABI flavour of that spelling: {}.\n",
        query_flavour.label()
    );

    if !same.is_empty() {
        if defined_same {
            out.push_str("  Status here: DEFINED.\n");
        } else {
            out.push_str(
                "  Status here: UNDEFINED (U) — this artifact CONSUMES the symbol, it does not \
                 provide it. The definition has to come from another object or library, listed \
                 AFTER this one on the link line.\n",
            );
        }
        out.push_str(&list_symbols(&same));
    } else if cross.is_empty() {
        out.push_str(
            "  Status here: ABSENT — nothing in this artifact matches that name under EITHER \
             libstdc++ ABI, so the dual ABI is not the explanation. Check that this is the right \
             library, that the definition was actually compiled into it, and that it appears \
             after its users on the link line.\n",
        );
        return out;
    } else {
        out.push_str("  Status here: ABSENT under that spelling.\n");
    }

    if !defined_same && !cross.is_empty() {
        let other = abi_flavour(&cross[0].name);
        let defined_cross = cross.iter().any(|s| s.is_defined());
        out.push_str(&format!(
            "\n  ⚠ libstdc++ DUAL-ABI MISMATCH. The same function {} here, spelled for the OTHER \
             ABI:\n",
            if defined_cross {
                "IS DEFINED"
            } else {
                "is referenced"
            }
        ));
        out.push_str(&list_symbols(&cross));
        out.push_str(&format!(
            "\n  This artifact is built for the {}, while the spelling you asked about is the \
             {}. Those two std::string types mangle differently, so the linker never matches \
             them and reports \"undefined reference\" even though the declaration is identical \
             in the header.\n\
             \x20 Fix — pick one side and make EVERY object in the link agree:\n\
             \x20   - compile the consumer with {} (matches this artifact; no rebuild of the \
             library needed), or\n\
             \x20   - rebuild this artifact with {} (matches the consumer).\n",
            other.label(),
            query_flavour.label(),
            other.consumer_flag(),
            query_flavour.consumer_flag(),
        ));
    }
    out
}

fn list_symbols(symbols: &[&NmSymbol]) -> String {
    let mut out = String::new();
    for sym in symbols.iter().take(ABI_MATCH_CAP) {
        out.push_str(&format!("      {} {}\n", sym.kind, sym.name));
    }
    if symbols.len() > ABI_MATCH_CAP {
        out.push_str(&format!(
            "      … and {} more match(es)\n",
            symbols.len() - ABI_MATCH_CAP
        ));
    }
    out
}

/// The always-printed half: what ABI this artifact as a whole was built for,
/// and how much of it is defined vs consumed. This is what lets the model
/// compare two artifacts without being told which symbol to look at.
fn render_posture(raw: &[NmSymbol], demangled: &[NmSymbol]) -> String {
    // Count over the demangled view when there is one: `std::string` and
    // `std::__cxx11::basic_string` are unambiguous there, while the mangled
    // `Ss` abbreviation needs the length-prefix heuristic. The two views list
    // the same symbols, so counting both would double every number.
    let symbols = if demangled.is_empty() { raw } else { demangled };
    if symbols.is_empty() {
        return "\nABI posture: no symbols at all — stripped, empty, or not an object file.\n"
            .to_string();
    }
    let total = symbols.len();
    let undefined = symbols.iter().filter(|s| s.is_undefined()).count();
    let defined = total - undefined;
    let cxx11 = symbols
        .iter()
        .filter(|s| abi_flavour(&s.name) == AbiFlavour::Cxx11)
        .count();
    let legacy = symbols
        .iter()
        .filter(|s| abi_flavour(&s.name) == AbiFlavour::Legacy)
        .count();
    let verdict = match (cxx11, legacy) {
        (0, 0) => "no std::string in any symbol, so the dual ABI cannot be the problem here"
            .to_string(),
        (c, 0) => format!(
            "built with -D_GLIBCXX_USE_CXX11_ABI=1 ({c} __cxx11 symbol(s), 0 old-ABI std::string)"
        ),
        (0, l) => format!(
            "built with -D_GLIBCXX_USE_CXX11_ABI=0 ({l} old-ABI std::string symbol(s), 0 __cxx11)"
        ),
        (c, l) => format!(
            "MIXED — {c} __cxx11 and {l} old-ABI std::string symbol(s) in one artifact, so part \
             of it was compiled the other way"
        ),
    };
    format!(
        "\nSymbols: {total} total, {defined} defined, {undefined} undefined (U).\n\
         ABI posture: {verdict}.\n"
    )
}

/// `objdump` argv. With a `symbol`, scope the disassembly to that one function
/// (`--disassemble=<symbol>`, binutils >= 2.32); otherwise disassemble the whole
/// binary (caller budgets the result). Always Intel syntax (`-M intel`) and
/// demangled (`-C`).
fn disasm_args(binary: &str, symbol: Option<&str>) -> Vec<String> {
    match symbol {
        Some(sym) => vec![
            format!("--disassemble={sym}"),
            "-M".to_string(),
            "intel".to_string(),
            "-C".to_string(),
            binary.to_string(),
        ],
        None => vec![
            "-d".to_string(),
            "-M".to_string(),
            "intel".to_string(),
            "-C".to_string(),
            binary.to_string(),
        ],
    }
}

/// What a binutils invocation produced. Mirrors the two fields of
/// `std::process::Output` this module ever read, so routing through
/// [`run_capped`] does not disturb the call sites.
#[derive(Debug)]
struct ToolOut {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run a binutils program under the shared wall-clock + byte caps.
///
/// `objdump -d` over a large binary is genuinely slow, so it must not be
/// an uncapped `.output()`. A timeout is surfaced as an error rather than as
/// empty output, because empty `nm` stdout is how this module detects a
/// *stripped* binary — a silent timeout would be reported as a fact about the
/// binary. Missing-binary and package hints now come from `run::spawn_error`.
async fn run_tool(
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<ToolOut, FunctionCallError> {
    match run_capped(program, args, cwd, timeout_from(None)).await? {
        RunOutcome::TimedOut { secs } => Err(FunctionCallError::RespondToModel(format!(
            "{program} timed out after {secs}s on this binary and was killed. Pass `symbol` to \
             scope the work to one function."
        ))),
        RunOutcome::Finished {
            stdout,
            mut stderr,
            truncated,
            success,
            code,
        } => {
            // Empty stdout reads as "stripped" / "no such section" here,
            // so a tool that crashed must not return its (empty) output.
            if let Some(note) = crate::run::crash_note(program, success, code) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{note} Nothing it printed is used. stderr: {}",
                    stderr.trim()
                )));
            }
            if truncated {
                stderr.push_str("\n(output was cut at the capture ceiling; it is incomplete)");
            }
            Ok(ToolOut {
                stdout: stdout.into_bytes(),
                stderr: stderr.into_bytes(),
            })
        }
    }
}

/// Merge a command's stdout and stderr into one display string. stderr is
/// appended (labeled) only when non-empty, so `nm`'s "no symbols" note and
/// `objdump`'s "no such symbol" surface to the model.
fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut text = stdout.trim_end().to_string();
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("stderr: ");
        text.push_str(stderr);
    }
    text
}

/// True when `nm` found no usable symbols — i.e. an empty stdout. A stripped
/// binary produces no symbol lines (and an "no symbols" note on stderr), which
/// triggers the dynamic-symbol fallback.
fn no_symbols(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).trim().is_empty()
}

/// Cap `text` to [`MAX_LINES`] lines and [`MAX_BYTES`] bytes, appending a
/// truncation note describing how much was dropped. Whole lines only — never a
/// partial line — so disassembly/section dumps stay readable.
fn budget(text: &str) -> String {
    let total_lines = text.lines().count();
    let mut out = String::new();
    let mut taken = 0usize;
    for line in text.lines() {
        // +1 for the newline we append.
        if taken >= MAX_LINES || out.len() + line.len() + 1 > MAX_BYTES {
            break;
        }
        out.push_str(line);
        out.push('\n');
        taken += 1;
    }
    if taken < total_lines {
        out.push_str(&format!(
            "… [truncated: showing {taken} of {total_lines} lines; output capped at {MAX_LINES} \
             lines / {MAX_BYTES} bytes]\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "a.out"), PathBuf::from("/work/a.out"));
        assert_eq!(resolve_path(cwd, "build/libx.so"), PathBuf::from("/work/build/libx.so"));
        assert_eq!(resolve_path(cwd, "/abs/bin"), PathBuf::from("/abs/bin"));
    }

    #[test]
    fn action_parses_known_values() {
        assert_eq!(Action::parse("disasm").unwrap(), Action::Disasm);
        assert_eq!(Action::parse("symbols").unwrap(), Action::Symbols);
        assert_eq!(Action::parse("headers").unwrap(), Action::Headers);
        assert_eq!(Action::parse("dynamic").unwrap(), Action::Dynamic);
        // FAILS ON OLD CODE: "abi" was not a variant, so this returned Err and
        // the tool description's link/ABI promise had no implementation.
        assert_eq!(Action::parse("abi").unwrap(), Action::Abi);
    }

    #[test]
    fn action_rejects_unknown_value() {
        let err = Action::parse("strings").unwrap_err();
        assert!(err.contains("unknown action"));
        assert!(err.contains("strings"));
        // The list the model is told to choose from must include the new mode.
        assert!(err.contains("abi"), "expected list to offer abi: {err}");
    }

    #[test]
    fn default_action_is_headers() {
        // serde default kicks in when `action` is omitted.
        let args: BinaryArgs = serde_json::from_str(r#"{"binary":"a.out"}"#).unwrap();
        assert_eq!(args.action, "headers");
        assert!(args.symbol.is_none());
        assert_eq!(Action::parse(&args.action).unwrap(), Action::Headers);
    }

    #[test]
    fn headers_argv_includes_header_program_headers_and_sections() {
        // -l is load-bearing: GNU_STACK (NX) and GNU_RELRO are program headers,
        // and the tool description promises they are inspectable.
        assert_eq!(
            headers_args("/b/a.out"),
            vec![
                "-h".to_string(),
                "-l".to_string(),
                "-S".to_string(),
                "/b/a.out".to_string()
            ]
        );
    }

    #[test]
    fn dynamic_argv_queries_dynamic_section() {
        assert_eq!(dynamic_args("/b/a.out"), vec!["-d".to_string(), "/b/a.out".to_string()]);
    }

    #[test]
    fn symbols_argv_is_demangled_and_keeps_imports() {
        let argv = symbols_args("/b/a.out", false);
        assert!(argv.contains(&"-C".to_string()), "demangle flag present: {argv:?}");
        // `U __stack_chk_fail` is how a stack canary shows.
        assert!(!argv.contains(&"--defined-only".to_string()));
        assert!(!argv.contains(&"-D".to_string()));
        assert_eq!(argv.last().unwrap(), "/b/a.out");
    }

    fn status_of(query: &str, raw: &str, demangled: &str) -> String {
        abi_body(Some(query), raw, demangled, false)
    }

    #[test]
    fn a_name_that_merely_contains_the_query_is_not_the_symbol() {
        // The four measured false answers of a substring test.
        let out = status_of("record", "", "0000000000001139 T recorder(double)\n");
        assert!(out.contains("ABSENT"), "record vs recorder: {out}");

        let raw = "                 U _Z6recordd\n0000000000001139 T _Z6recorddi\n";
        let out = status_of("_Z6recordd", raw, "");
        assert!(out.contains("UNDEFINED") && !out.contains("DEFINED."), "mangled: {out}");

        let out = status_of("record(double)", "", "0000000000001139 T metrics::record(double)\n");
        assert!(out.contains("ABSENT"), "signature vs a namespaced one: {out}");

        let dem = "0000000000001139 T metrics::log(std::__cxx11::basic_string<char, \
                   std::char_traits<char>, std::allocator<char> > const&)\n";
        let out = status_of("log(std::string const&)", "", dem);
        assert!(!out.contains("MISMATCH"), "a different function is no ABI mismatch: {out}");
    }

    #[test]
    fn an_ss_inside_an_identifier_is_not_the_old_string_abbreviation() {
        // `_Z8checkSsli` is `checkSsl(int)`; its `Ss` is part of the name.
        assert_eq!(abi_flavour("_Z8checkSsli"), abi_flavour("_Z1fi"));
        assert!(!replace_legacy_abbrev("_Z8checkSsli").contains(ABI_STRING_PLACEHOLDER));
        // The real abbreviation is still found, after names and after
        // substitutions: `metrics::record(std::string const&, double)`.
        assert!(replace_legacy_abbrev("_ZN7metrics6recordERKSsd").contains(ABI_STRING_PLACEHOLDER));
        assert!(replace_legacy_abbrev("_Z1fS0_Ss").contains(ABI_STRING_PLACEHOLDER));
        assert!(!replace_legacy_abbrev("_ZN2Ss3fooEv").contains(ABI_STRING_PLACEHOLDER));
    }

    #[test]
    fn a_versioned_dynamic_symbol_is_the_same_symbol() {
        // Found replaying recorded cases: `U __stack_chk_fail@GLIBC_2.4` must
        // still answer the canary question (UNDEFINED, not ABSENT).
        let raw = "                 U __stack_chk_fail@GLIBC_2.4\n";
        let out = status_of("__stack_chk_fail", raw, raw);
        assert!(out.contains("UNDEFINED"), "{out}");
        assert_eq!(without_symbol_version("f@@GLIBCXX_3.4"), "f");
        assert_eq!(without_symbol_version("ns::f(int)@GLIBC_2.2"), "ns::f(int)");
        assert_eq!(without_symbol_version("plain"), "plain");
    }

    #[test]
    fn a_bare_name_still_finds_every_overload_on_a_name_boundary() {
        let dem = "0000000000001139 T metrics::record(double)\n\
                   0000000000001150 T metrics::record(double, int)\n";
        for query in ["metrics::record", "record"] {
            let out = status_of(query, "", dem);
            assert!(out.contains("DEFINED"), "{query}: {out}");
            assert!(out.contains("record(double, int)"), "{query}: {out}");
        }
    }

    #[test]
    fn an_unreadable_file_is_not_called_stripped() {
        // nm's error must not be reported as "binary is stripped".
        let msg = no_symbols_message("nm: notes.txt: file format not recognized");
        assert!(msg.contains("could not read") && !msg.contains("stripped"), "{msg}");
        assert!(no_symbols_message("").contains("stripped"));
    }

    #[test]
    fn symbols_argv_dynamic_fallback_uses_dash_d_and_demangles() {
        let argv = symbols_args("/b/a.out", true);
        assert!(argv.contains(&"-D".to_string()), "dynamic-symbol flag: {argv:?}");
        assert!(argv.contains(&"-C".to_string()), "demangle flag present: {argv:?}");
        assert!(!argv.contains(&"--defined-only".to_string()));
    }

    #[test]
    fn disasm_argv_scopes_to_symbol_when_given() {
        let argv = disasm_args("/b/a.out", Some("foo::bar(int)"));
        assert_eq!(argv[0], "--disassemble=foo::bar(int)");
        assert!(argv.contains(&"-M".to_string()) && argv.contains(&"intel".to_string()));
        assert!(argv.contains(&"-C".to_string()), "demangle flag present: {argv:?}");
        assert!(!argv.contains(&"-d".to_string()), "scoped disasm must not pass -d: {argv:?}");
        assert_eq!(argv.last().unwrap(), "/b/a.out");
    }

    #[test]
    fn disasm_argv_full_when_no_symbol() {
        let argv = disasm_args("/b/a.out", None);
        assert!(argv.contains(&"-d".to_string()), "full disasm uses -d: {argv:?}");
        assert!(argv.iter().all(|a| !a.starts_with("--disassemble=")));
        assert!(argv.contains(&"-M".to_string()) && argv.contains(&"intel".to_string()));
        assert!(argv.contains(&"-C".to_string()));
    }

    #[test]
    fn empty_symbol_is_treated_as_none_by_filter() {
        // Mirrors the handler's `.filter(|s| !s.is_empty())` after trim.
        let symbol: Option<&str> = Some("   ").map(str::trim).filter(|s| !s.is_empty());
        assert!(symbol.is_none());
        // ...and an empty-symbol disasm must be a full disasm, not `--disassemble=`.
        let argv = disasm_args("/b/a.out", symbol);
        assert!(argv.contains(&"-d".to_string()));
    }

    #[test]
    fn budget_caps_long_output_and_notes_truncation() {
        let big: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let out = budget(&big);
        // Body lines are capped at MAX_LINES; the note adds one extra line.
        let line_count = out.lines().count();
        assert!(line_count <= MAX_LINES + 1, "got {line_count} lines");
        assert!(out.contains("truncated"));
        assert!(out.contains("showing 200 of 500 lines"));
        assert!(out.len() <= MAX_BYTES + 256, "byte cap respected: {}", out.len());
    }

    #[test]
    fn budget_passes_short_output_through_unchanged_without_note() {
        let small = "Type: DYN\nMachine: X86-64\n";
        let out = budget(small);
        assert!(!out.contains("truncated"));
        assert_eq!(out, small);
    }

    #[test]
    fn budget_respects_byte_cap_even_with_few_lines() {
        // A handful of very long lines exceeds MAX_BYTES before MAX_LINES.
        let huge_line = "x".repeat(MAX_BYTES);
        let text = format!("{huge_line}\n{huge_line}\n{huge_line}\n");
        let out = budget(&text);
        assert!(out.contains("truncated"));
    }

    #[test]
    fn combine_output_appends_labeled_stderr() {
        let text = combine_output(b"main\nfoo\n", b"nm: a.out: no symbols");
        assert!(text.starts_with("main\nfoo"));
        assert!(text.contains("stderr: nm: a.out: no symbols"));
    }

    #[test]
    fn combine_output_no_stderr_label_when_empty() {
        assert_eq!(combine_output(b"just stdout\n", b""), "just stdout");
    }

    #[test]
    fn no_symbols_true_on_stripped_diagnostic() {
        assert!(no_symbols(b""));
        assert!(no_symbols(b"   \n"));
    }

    #[test]
    fn no_symbols_false_when_stdout_has_content() {
        assert!(!no_symbols(b"0000 T main\n"));
    }

    // ---------------------------------------------------------------------
    // action = "abi"
    //
    // Every fixture below is REAL `nm` output, captured from objects compiled
    // from the same source with -D_GLIBCXX_USE_CXX11_ABI=0 and =1, not invented.
    // Every test in this block FAILS ON OLD CODE for the same root reason: the
    // `abi` action and all of its helpers did not exist — the tool description
    // promised "`nm -C` the archive to see which symbol is missing or
    // differently mangled" and left the model to eyeball a `symbols` dump.
    // ---------------------------------------------------------------------

    /// `nm libold.a` — archive built with -D_GLIBCXX_USE_CXX11_ABI=0.
    const OLD_ABI_RAW: &str = "\nm_old.o:\n0000000000000000 T _ZN7metrics6recordERKSsd\n";
    /// `nm -C libold.a` — the same archive.
    const OLD_ABI_DEM: &str =
        "\nm_old.o:\n0000000000000000 T metrics::record(std::string const&, double)\n";
    /// `nm m_new.o` — the same source with -D_GLIBCXX_USE_CXX11_ABI=1.
    const NEW_ABI_RAW: &str = "0000000000000000 T _ZN7metrics6recordERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEd\n";
    /// `nm -C m_new.o`.
    const NEW_ABI_DEM: &str = "0000000000000000 T metrics::record(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, double)\n";
    /// The demangled spelling a consumer's linker error quotes.
    const CXX11_QUERY: &str = "metrics::record(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, double)";

    #[test]
    fn abi_argv_keeps_undefined_symbols_and_demangles_on_request() {
        // No --defined-only, unlike symbols_args: the U entries ARE the
        // evidence for a link failure, so filtering them removes the answer.
        let raw = abi_args("/b/libx.a", false, false);
        assert_eq!(raw, vec!["/b/libx.a".to_string()]);
        let dem = abi_args("/b/libx.a", true, false);
        assert_eq!(dem, vec!["-C".to_string(), "/b/libx.a".to_string()]);
        let dynamic = abi_args("/b/libx.so", true, true);
        assert_eq!(
            dynamic,
            vec!["-D".to_string(), "-C".to_string(), "/b/libx.so".to_string()]
        );
        for argv in [raw, dem, dynamic] {
            assert!(
                !argv.contains(&"--defined-only".to_string()),
                "abi must see undefined symbols: {argv:?}"
            );
        }
    }

    #[test]
    fn parse_nm_reads_archive_members_addresses_and_undefined_lines() {
        // The blank address column on the `U` line is significant — it is how
        // nm prints an undefined symbol — so build the fixture without a
        // line-continuation, which would eat the leading spaces.
        let text = concat!(
            "\n",
            "m_old.o:\n",
            "0000000000000000 T _ZN7metrics6recordERKSsd\n",
            "                 U printf\n",
        );
        let syms = parse_nm(text);
        assert_eq!(syms.len(), 2, "member header must not become a symbol: {syms:?}");
        assert_eq!(syms[0].kind, 'T');
        assert_eq!(syms[0].name, "_ZN7metrics6recordERKSsd");
        assert!(syms[0].is_defined());
        assert_eq!(syms[1].kind, 'U');
        assert_eq!(syms[1].name, "printf");
        assert!(syms[1].is_undefined());
    }

    #[test]
    fn abi_base_collapses_both_spellings_of_std_string() {
        // The load-bearing equality: the same function, compiled either way,
        // must normalize to one string — in the demangled AND mangled view.
        assert_eq!(
            abi_base("metrics::record(std::string const&, double)"),
            abi_base(CXX11_QUERY)
        );
        assert_eq!(
            abi_base("_ZN7metrics6recordERKSsd"),
            abi_base("_ZN7metrics6recordERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEd")
        );
        // A signature with no string is left alone apart from whitespace.
        assert_eq!(abi_base("foo::bar(int, double)"), "foo::bar(int,double)");
    }

    #[test]
    fn ss_abbreviation_is_not_confused_with_a_user_type_named_ss() {
        // Structural, not a guess: in a mangled name an identifier is preceded
        // by its length, so `2Ss` is a class called Ss, while `RKSs` is a
        // reference to the old-ABI std::string.
        assert_eq!(abi_flavour("_ZN7metrics6recordERKSsd"), AbiFlavour::Legacy);
        assert_eq!(abi_flavour("_ZN2Ss3fooEv"), AbiFlavour::Neutral);
        // And in a DEMANGLED name, `Ss` is just two letters.
        assert_eq!(abi_flavour("crypto::Ssl_init()"), AbiFlavour::Neutral);
        assert_eq!(abi_flavour(CXX11_QUERY), AbiFlavour::Cxx11);
        assert_eq!(
            abi_flavour("metrics::record(std::string const&, double)"),
            AbiFlavour::Legacy
        );
    }

    #[test]
    fn old_abi_archive_against_a_cxx11_query_is_named_a_dual_abi_mismatch() {
        // A real case: `undefined reference to metrics::record`, caused by a
        // vendor .a built with -D_GLIBCXX_USE_CXX11_ABI=0. Before this action
        // it was solved only by shelling out to nm and reading the mangling.
        let body = abi_body(Some(CXX11_QUERY), OLD_ABI_RAW, OLD_ABI_DEM, false);
        assert!(body.contains("DUAL-ABI MISMATCH"), "{body}");
        assert!(
            body.contains("metrics::record(std::string const&, double)"),
            "must show the symbol the artifact really exports: {body}"
        );
        assert!(
            body.contains("-D_GLIBCXX_USE_CXX11_ABI=0"),
            "must name the fix for the consumer: {body}"
        );
        assert!(
            body.contains("-D_GLIBCXX_USE_CXX11_ABI=1"),
            "must also offer rebuilding the library: {body}"
        );
    }

    #[test]
    fn a_mangled_query_copied_from_the_linker_also_finds_the_mismatch() {
        let body = abi_body(
            Some("_ZN7metrics6recordERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEd"),
            OLD_ABI_RAW,
            OLD_ABI_DEM,
            false,
        );
        assert!(body.contains("DUAL-ABI MISMATCH"), "{body}");
        assert!(body.contains("_ZN7metrics6recordERKSsd"), "{body}");
    }

    #[test]
    fn matching_abi_is_reported_as_defined_not_as_a_mismatch() {
        let body = abi_body(Some(CXX11_QUERY), NEW_ABI_RAW, NEW_ABI_DEM, false);
        assert!(body.contains("DEFINED"), "{body}");
        assert!(
            !body.contains("MISMATCH"),
            "a matching ABI must not be called a mismatch: {body}"
        );
    }

    #[test]
    fn absent_symbol_is_reported_absent_not_as_a_mismatch() {
        let body = abi_body(Some("frobnicate(int)"), OLD_ABI_RAW, OLD_ABI_DEM, false);
        assert!(body.contains("ABSENT"), "{body}");
        assert!(
            !body.contains("MISMATCH"),
            "a missing symbol is not an ABI problem: {body}"
        );
        assert!(
            body.contains("link line"),
            "must point at the real causes instead: {body}"
        );
    }

    #[test]
    fn a_symbol_only_referenced_here_is_reported_as_consumed() {
        // The consumer side of the same link: it has U, not T. Saying "found
        // it" here would send the model looking in the wrong artifact.
        let raw = "                 U _ZN7metrics6recordERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEd\n0000000000000000 T main\n";
        let dem = format!("                 U {CXX11_QUERY}\n0000000000000000 T main\n");
        let body = abi_body(Some(CXX11_QUERY), raw, &dem, false);
        assert!(body.contains("UNDEFINED (U)"), "{body}");
        assert!(!body.contains("MISMATCH"), "{body}");
        assert!(body.contains("CONSUMES"), "{body}");
    }

    #[test]
    fn posture_summary_without_a_symbol_names_the_build_flag() {
        let body = abi_body(None, OLD_ABI_RAW, OLD_ABI_DEM, false);
        assert!(
            body.contains("-D_GLIBCXX_USE_CXX11_ABI=0"),
            "posture must state which ABI this artifact was built for: {body}"
        );
        assert!(body.contains("1 total"), "{body}");
        assert!(body.contains("1 defined"), "{body}");
        assert!(
            body.contains("compare"),
            "must tell the model how to use it on two artifacts: {body}"
        );
    }

    #[test]
    fn posture_calls_out_an_artifact_holding_both_abis() {
        let dem = format!("{OLD_ABI_DEM}{NEW_ABI_DEM}");
        let raw = format!("{OLD_ABI_RAW}{NEW_ABI_RAW}");
        let body = abi_body(None, &raw, &dem, false);
        assert!(body.contains("MIXED"), "{body}");
    }

    #[test]
    fn dynamic_fallback_is_stated_so_an_empty_result_is_not_mistaken_for_a_fact() {
        let body = abi_body(None, "", "", true);
        assert!(body.contains("nm -D"), "{body}");
        assert!(body.contains("no symbols at all"), "{body}");
    }

    #[tokio::test]
    async fn missing_tool_maps_to_respond_to_model() {
        let err = run_tool(
            "definitely-not-a-real-binutils-xyz",
            &["-h".to_string()],
            &std::env::temp_dir(),
        )
        .await
        .unwrap_err();
        match err {
            FunctionCallError::RespondToModel(msg) => {
                assert!(msg.contains("is not installed / not on PATH"), "msg: {msg}");
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
    }
}
