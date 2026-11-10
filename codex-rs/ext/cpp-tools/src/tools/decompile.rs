use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

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
use serde_json::json;

use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;

const TOOL_NAME: &str = "cpp_decompile";
const DEFAULT_ACTION: &str = "decompile";
const DEFAULT_FUNCTION: &str = "main";
/// Cap on the number of output lines handed back to the model.
const MAX_LINES: usize = 250;
/// Functions listed when a name matches several, or none (close matches).
const MAX_CANDIDATES: usize = 10;
/// The r2ghidra probe opens no file and runs no analysis (~20 ms).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Note prepended when r2ghidra's `pdg` decompiler is unavailable and we fell
/// back to plain disassembly (`pdf`).
const FALLBACK_NOTE: &str = "Note: r2ghidra (the `pdg` decompiler plugin) is not installed; \
showing disassembly (`pdf`) instead.";
/// r2ghidra IS loaded but gave nothing usable for this function — do not claim
/// it is missing.
const PDG_FAILED_NOTE: &str = "Note: r2ghidra's `pdg` produced no pseudocode for this function; \
showing disassembly (`pdf`) instead.";
/// r2 function-name prefixes (flag spaces). A person types `leaf`, r2 lists
/// `dbg.leaf(int*)`. `sym.imp.` must precede `sym.`.
const R2_PREFIXES: [&str; 4] = ["dbg.", "sym.imp.", "sym.", "method."];

pub struct DecompileTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecompileArgs {
    binary: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    function: Option<String>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for DecompileTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "binary".to_string(),
            JsonSchema::string(Some(
                "Path to the compiled/stripped/closed binary to analyze, relative to the \
                 workspace or absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("decompile"),
                    json!("disasm"),
                    json!("functions"),
                    json!("info"),
                ],
                Some(
                    "\"decompile\" (default) emits C-like pseudocode for one function via \
                     r2ghidra (`pdg`), falling back to disassembly when r2ghidra is absent; \
                     \"disasm\" disassembles one function (`pdf`); \"functions\" lists analyzed \
                     functions; \"info\" shows binary metadata (arch, bits, PIE, stripped)."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "function".to_string(),
            JsonSchema::string(Some(format!(
                "Function to target for \"decompile\"/\"disasm\": a symbol name or address \
                 (default \"{DEFAULT_FUNCTION}\"). Ignored for \"functions\"/\"info\"."
            ))),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Decompile or disassemble a function in a compiled/stripped/closed \
                binary using radare2 (decompile via r2ghidra's pdg when available, else \
                disassembly via pdf), list functions, or show binary info. For reverse-engineering \
                / understanding binaries without source. C/C++ binaries only."
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

/// What the caller's `function` names, decided before r2 is asked anything.
#[derive(Debug, PartialEq)]
enum Target {
    /// `0x…` hex or plain decimal.
    Address(u64),
    /// Anything else: looked up in r2's function list, never sent to r2.
    Name(String),
}

/// A function address plus the line saying how it was found (empty for an
/// address the caller gave), or the answer to give instead of disassembling.
enum Located {
    At { addr: u64, header: String },
    Unresolved(String),
}

/// One entry of r2's `aflj` function list.
#[derive(Debug, Clone, PartialEq)]
struct R2Function {
    name: String,
    addr: u64,
}

#[derive(Debug, PartialEq)]
enum Resolution<'a> {
    Found(&'a R2Function),
    Ambiguous(Vec<&'a R2Function>),
    /// Nothing matched; carries close matches to offer instead.
    Missing(Vec<&'a R2Function>),
}

impl DecompileTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: DecompileArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let body = self.run(&args).await?;
        // Decompiled C is a reconstruction, never the original source.
        let result = check_result::CheckResult::approximate(
            TOOL_NAME,
            "radare2's output is a reconstruction from machine code, not the original source.",
        );
        Ok(crate::report::report(TOOL_NAME, &args.binary, body, result))
    }

    async fn run(&self, args: &DecompileArgs) -> Result<String, FunctionCallError> {
        let action = action_or_default(args.action.as_deref());
        if !is_valid_action(action) {
            return Err(FunctionCallError::RespondToModel(format!(
                "invalid action {action:?}; expected \"decompile\", \"disasm\", \"functions\", or \"info\""
            )));
        }
        let binary = resolve_path(&self.cwd, &args.binary);
        if !binary.is_file() {
            return Err(FunctionCallError::RespondToModel(format!(
                "binary not found (or not a file): {}",
                binary.display()
            )));
        }
        if let Some(command) = listing_command(action) {
            return Ok(budget(&self.run_r2(command, &binary).await?, MAX_LINES));
        }

        let function = function_or_default(args.function.as_deref());
        let addr = match parse_target(&function) {
            Target::Address(addr) => addr,
            Target::Name(name) => {
                let cap = timeout_from(None);
                return match tokio::time::timeout(cap, self.named(&name, action, &binary)).await {
                    Ok(result) => result,
                    Err(_) => Ok(timed_out(cap.as_secs())),
                };
            }
        };
        if action == "disasm" {
            Ok(budget(
                &self.run_r2(&at_address("pdf", addr), &binary).await?,
                MAX_LINES,
            ))
        } else {
            self.decompile(addr, &binary).await
        }
    }

    /// A function given by NAME, in one r2 process: the analysis runs once,
    /// then `aflj` resolves the name and `pdg`/`pdf` answers from the same
    /// analysis. Two separate runs (the list, then the code) each paid for a
    /// full `-A` analysis — minutes on a large binary.
    async fn named(
        &self,
        name: &str,
        action: &str,
        binary: &Path,
    ) -> Result<String, FunctionCallError> {
        let use_pdg = action != "disasm" && self.pdg_installed().await?;
        let mut r2 = R2Session::open(binary, &self.cwd).await?;
        if let Err(failure) = r2.cmd("aaa").await {
            return Ok(failure.answer());
        }
        let listing = match r2.cmd("aflj").await {
            Ok(text) => text,
            Err(failure) => return Ok(failure.answer()),
        };
        let Some(functions) = parse_function_list(&listing) else {
            return Ok(format!(
                "Could not read r2's function list to find {name:?}, so nothing was \
                 disassembled. Pass the function's address (0x…) instead. r2 said:\n{}",
                listing.trim()
            ));
        };
        let (addr, header) = match located(name, &functions) {
            Located::At { addr, header } => (addr, header),
            Located::Unresolved(answer) => return Ok(answer),
        };
        let pdf = at_address("pdf", addr);
        let body = if action == "disasm" {
            match r2.cmd(&pdf).await {
                Ok(text) => budget(&text, MAX_LINES),
                Err(failure) => return Ok(failure.answer()),
            }
        } else if !use_pdg {
            match r2.cmd(&pdf).await {
                Ok(text) => format!("{FALLBACK_NOTE}\n\n{}", budget(&text, MAX_LINES)),
                Err(failure) => return Ok(failure.answer()),
            }
        } else {
            match r2.cmd(&at_address("pdg", addr)).await {
                Ok(text) if !pdg_unavailable(&text) => budget(&text, MAX_LINES),
                Ok(_) => match r2.cmd(&pdf).await {
                    Ok(text) => format!("{PDG_FAILED_NOTE}\n\n{}", budget(&text, MAX_LINES)),
                    Err(failure) => return Ok(failure.answer()),
                },
                Err(failure) => return Ok(failure.answer()),
            }
        };
        Ok(format!("{header}{body}"))
    }

    /// `decompile` at `addr`: r2ghidra's `pdg` when it is loaded, else `pdf`.
    async fn decompile(&self, addr: u64, binary: &Path) -> Result<String, FunctionCallError> {
        let disassemble = at_address("pdf", addr);
        if !self.pdg_installed().await? {
            // Probing first costs ~20 ms; asking `pdg` to find out costs a
            // whole `-A` analysis (47 s on a 5 MB binary), and its "Missing
            // plugin" error is easy to miss, in which case nothing falls back at all.
            let pdf = self.run_r2(&disassemble, binary).await?;
            return Ok(format!("{FALLBACK_NOTE}\n\n{}", budget(&pdf, MAX_LINES)));
        }
        let pdg = self.run_r2(&at_address("pdg", addr), binary).await?;
        if pdg_unavailable(&pdg) {
            let pdf = self.run_r2(&disassemble, binary).await?;
            return Ok(format!("{PDG_FAILED_NOTE}\n\n{}", budget(&pdf, MAX_LINES)));
        }
        Ok(budget(&pdg, MAX_LINES))
    }

    /// Is the r2ghidra plugin loaded? `pdg?` on no file answers without
    /// analysing anything.
    async fn pdg_installed(&self) -> Result<bool, FunctionCallError> {
        let argv = ["-e", "scr.color=0", "-q", "-c", "pdg?", "--"].map(str::to_string);
        Ok(
            match run_capped("r2", &argv, &self.cwd, PROBE_TIMEOUT).await? {
                RunOutcome::Finished {
                    success: true,
                    stdout,
                    stderr,
                    ..
                } => !pdg_unavailable(&combine_output(stdout.as_bytes(), stderr.as_bytes())),
                _ => false,
            },
        )
    }

    /// radare2's analysis (`aaa`) on a large stripped binary can run for
    /// many minutes; a raw `.output()` would give the model no way out of it.
    async fn run_r2(&self, command: &str, binary: &Path) -> Result<String, FunctionCallError> {
        let argv = r2_args(command, binary);
        match run_capped("r2", &argv, &self.cwd, timeout_from(None)).await? {
            RunOutcome::TimedOut { secs } => Ok(timed_out(secs)),
            RunOutcome::Finished {
                stdout,
                stderr,
                success,
                code,
                ..
            } => {
                let output = combine_output(stdout.as_bytes(), stderr.as_bytes());
                // A SIGSEGV'd r2 must not come back as an empty "decompilation".
                Ok(match crate::run::crash_note("r2", success, code) {
                    Some(note) => format!("{note}\n{output}"),
                    None => output,
                })
            }
        }
    }
}

/// Turn a function NAME into an address from r2's own function list. An
/// address is the ONLY way the caller's text influences an r2 command — see
/// [`at_address`].
fn located(name: &str, functions: &[R2Function]) -> Located {
    match resolve(name, functions) {
        Resolution::Found(found) => Located::At {
            addr: found.addr,
            header: format!("function: {} @ 0x{:x}\n\n", found.name, found.addr),
        },
        Resolution::Ambiguous(matches) => Located::Unresolved(format!(
            "{name:?} matches {} functions in this binary, so none was picked. Call again \
             with one of these addresses:\n{}",
            matches.len(),
            list_functions(&matches)
        )),
        Resolution::Missing(close) if close.is_empty() => Located::Unresolved(format!(
            "No function named {name:?} in r2's function list for this binary. action \
             \"functions\" lists every function r2 found; pass one of those names, or an \
             address (0x…)."
        )),
        Resolution::Missing(close) => Located::Unresolved(format!(
            "No function named {name:?} in r2's function list for this binary. Close \
             matches (pass the address):\n{}",
            list_functions(&close)
        )),
    }
}

/// One r2 process driven over its pipe protocol (`-q0`: every command's
/// output ends with a NUL byte), so a whole call pays for one analysis.
/// Killed with its process group when dropped.
struct R2Session {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

/// Why a session command produced no output.
struct R2Died {
    stderr: String,
}

impl R2Died {
    /// The answer to give instead (a crash is named as one).
    fn answer(&self) -> String {
        format!(
            "{} Nothing was disassembled. r2 said: {}",
            crate::run::crash_note("r2", false, None).unwrap_or_default(),
            self.stderr.trim()
        )
    }
}

impl R2Session {
    async fn open(binary: &Path, cwd: &Path) -> Result<Self, FunctionCallError> {
        use tokio::io::AsyncReadExt as _;
        let argv = vec![
            "-q0".to_string(),
            "-e".to_string(),
            "scr.color=0".to_string(),
            binary.display().to_string(),
        ];
        // Inside this tool's sandbox profile.
        let wrapped = crate::sandbox::wrap("r2", &argv, cwd)?;
        let mut command = tokio::process::Command::new(&wrapped.program);
        command
            .args(&wrapped.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|err| crate::run::spawn_error("r2", cwd, err))?;
        let (Some(stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(FunctionCallError::RespondToModel(
                "r2 started without its pipes".to_string(),
            ));
        };
        let tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = tail.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                if let Ok(mut text) = sink.lock() {
                    text.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if text.len() > 4000 {
                        let cut = text.len() - 4000;
                        let cut = (cut..text.len())
                            .find(|&i| text.is_char_boundary(i))
                            .unwrap_or(0);
                        text.drain(..cut);
                    }
                }
            }
        });
        let mut session = R2Session {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
            stderr: tail,
        };
        // r2 announces it is ready with a NUL of its own.
        if let Err(died) = session.read_reply().await {
            return Err(FunctionCallError::RespondToModel(died.answer()));
        }
        Ok(session)
    }

    async fn cmd(&mut self, command: &str) -> Result<String, R2Died> {
        use tokio::io::AsyncWriteExt as _;
        let line = format!("{command}\n");
        if self.stdin.write_all(line.as_bytes()).await.is_err() || self.stdin.flush().await.is_err()
        {
            return Err(self.died());
        }
        self.read_reply().await
    }

    async fn read_reply(&mut self) -> Result<String, R2Died> {
        use tokio::io::AsyncBufReadExt as _;
        let mut buf = Vec::new();
        match self.stdout.read_until(0, &mut buf).await {
            Ok(_) if buf.last() == Some(&0) => {
                buf.pop();
                Ok(String::from_utf8_lossy(&buf).into_owned())
            }
            _ => Err(self.died()),
        }
    }

    fn died(&self) -> R2Died {
        R2Died {
            stderr: self.stderr.lock().map(|s| s.clone()).unwrap_or_default(),
        }
    }
}

impl Drop for R2Session {
    fn drop(&mut self) {
        crate::run::kill_process_group(self.child.id());
    }
}

fn timed_out(secs: u64) -> String {
    format!(
        "r2 timed out after {secs}s (its process group was killed). Analysis of a large or \
         stripped binary can exceed this; target a specific function, or use cpp_binary for a \
         plain objdump/nm view."
    )
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

fn action_or_default(action: Option<&str>) -> &str {
    match action {
        Some(a) if !a.trim().is_empty() => a,
        _ => DEFAULT_ACTION,
    }
}

fn is_valid_action(action: &str) -> bool {
    matches!(action, "decompile" | "disasm" | "functions" | "info")
}

fn function_or_default(function: Option<&str>) -> String {
    match function {
        Some(f) if !f.trim().is_empty() => f.trim().to_string(),
        _ => DEFAULT_FUNCTION.to_string(),
    }
}

/// The fixed r2 command for the actions that target no function.
fn listing_command(action: &str) -> Option<&'static str> {
    match action {
        "functions" => Some("afl"),
        "info" => Some("iI"),
        _ => None,
    }
}

/// `pdf @ 0x13c0` — the only form in which a function reaches r2.
///
/// r2 parses `-c` as its own command language — `;` chains commands, `!`
/// runs a shell, a backtick substitutes a command's output, `>` redirects to a
/// file, `|` pipes to a shell, `~` greps. The caller's `function` used to be
/// pasted in verbatim, and it was measured doing all of those: `main; !touch
/// X` ran `touch`, `` `!touch X` `` ran it via substitution, and the C++ name
/// `std::vector<int>::push_back` created a file named `::push_back` in the
/// workspace. An address formatted here can contain none of that. (r2's
/// `cfg.sandbox` is no substitute: set before opening, it refuses the absolute
/// binary path; set after, it still let a backtick `!` run.)
fn at_address(command: &str, addr: u64) -> String {
    format!("{command} @ 0x{addr:x}")
}

/// `0x`-prefixed hex or plain decimal is an address; anything else is a name.
fn parse_target(function: &str) -> Target {
    let text = function.trim();
    let hex = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"));
    let addr = match hex {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()) => {
            u64::from_str_radix(digits, 16).ok()
        }
        None if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => text.parse().ok(),
        _ => None,
    };
    match addr {
        Some(addr) => Target::Address(addr),
        None => Target::Name(text.to_string()),
    }
}

/// Parse r2's `aflj`. `None` when the output is not a JSON array at all (r2
/// could not open the file, or the list overflowed the capture cap).
fn parse_function_list(stdout: &str) -> Option<Vec<R2Function>> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let functions = value
        .as_array()?
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            // r2 5.9 calls it `offset`; later releases rename it `addr`.
            let addr = entry
                .get("offset")
                .or_else(|| entry.get("addr"))?
                .as_u64()?;
            Some(R2Function { name, addr })
        })
        .collect();
    Some(functions)
}

/// Find the function `query` names in r2's list, most exact first: the name as
/// r2 lists it (`dbg.leaf(int*)`), then without r2's prefix (`leaf(int*)`),
/// then — only when the query has no parameter list — the bare name (`leaf`),
/// which matches every overload. A query WITH a parameter list never falls back
/// to the bare name: `f(long)` must not silently become the only `f(int)`.
/// Whitespace is ignored throughout (`int *` vs `int*`, `> >` vs `>>`).
fn resolve<'a>(query: &str, functions: &'a [R2Function]) -> Resolution<'a> {
    let bare_query = strip_r2_prefix(query.trim());
    let mut levels: Vec<(String, fn(&str) -> String)> = vec![
        (squash(query), |name| squash(name)),
        (squash(bare_query), |name| squash(strip_r2_prefix(name))),
    ];
    if !bare_query.contains('(') {
        levels.push((squash(bare_query), |name| {
            squash(without_params(strip_r2_prefix(name)))
        }));
    }
    for (wanted, key) in levels {
        let mut hits: Vec<&R2Function> = functions
            .iter()
            .filter(|f| key(&f.name) == wanted)
            .collect();
        hits.sort_by_key(|f| f.addr);
        hits.dedup_by_key(|f| f.addr);
        match hits.len() {
            0 => continue,
            1 => return Resolution::Found(hits[0]),
            _ => return Resolution::Ambiguous(hits),
        }
    }
    Resolution::Missing(close_matches(query, functions))
}

/// Functions whose name contains the query's last `::` component, so a
/// near-miss (`std::vector<int>::push_back`) still leads somewhere.
fn close_matches<'a>(query: &str, functions: &'a [R2Function]) -> Vec<&'a R2Function> {
    let base = without_params(strip_r2_prefix(query.trim()));
    let last = base
        .rsplit("::")
        .next()
        .unwrap_or(base)
        .trim()
        .to_lowercase();
    if last.is_empty() {
        return Vec::new();
    }
    functions
        .iter()
        .filter(|f| f.name.to_lowercase().contains(&last))
        .take(MAX_CANDIDATES)
        .collect()
}

fn list_functions(functions: &[&R2Function]) -> String {
    let mut out: Vec<String> = functions
        .iter()
        .take(MAX_CANDIDATES)
        .map(|f| format!("  0x{:x}  {}", f.addr, f.name))
        .collect();
    if functions.len() > MAX_CANDIDATES {
        out.push(format!("  … and {} more", functions.len() - MAX_CANDIDATES));
    }
    out.join("\n")
}

fn strip_r2_prefix(name: &str) -> &str {
    R2_PREFIXES
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name)
}

/// `ns::f(int) const` -> `ns::f`: the name without its trailing parameter list
/// (and any qualifiers after it). Names without one come back unchanged.
fn without_params(name: &str) -> &str {
    let name = name.trim_end();
    let Some(close) = name.rfind(')') else {
        return name;
    };
    let qualifiers_only = name[close + 1..]
        .split(|c: char| c.is_whitespace() || c == '&')
        .all(|word| matches!(word, "" | "const" | "volatile" | "noexcept"));
    if !qualifiers_only {
        return name;
    }
    let mut depth = 0usize;
    for (at, c) in name[..=close].char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return name[..at].trim_end();
                }
            }
            _ => {}
        }
    }
    name
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Build the radare2 argv: `-e scr.color=0 -q -c <command> -A <binary>` (`-A`
/// analyzes, `-q` quits after running `-c`, `scr.color=0` keeps ANSI escapes
/// out of the model's tokens). `command` is a fixed string from this module or
/// an [`at_address`] — never the caller's text.
fn r2_args(command: &str, binary: &Path) -> Vec<String> {
    vec![
        "-e".to_string(),
        "scr.color=0".to_string(),
        "-q".to_string(),
        "-c".to_string(),
        command.to_string(),
        "-A".to_string(),
        binary.to_string_lossy().into_owned(),
    ]
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(stdout));
    let err = String::from_utf8_lossy(stderr);
    if !err.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&err);
    }
    combined
}

/// Decide whether r2ghidra's `pdg` produced usable pseudocode. Empty output or
/// an r2 "unknown command" / "missing plugin" style error means it did not, so
/// the caller should fall back to disassembly.
fn pdg_unavailable(output: &str) -> bool {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    const ERROR_MARKERS: [&str; 6] = [
        "no such command",
        "unknown command",
        "command not found",
        "cannot find",
        "invalid command",
        // r2 >= 5.9: `ERROR: Missing plugin. Run: r2pm -ci r2ghidra`.
        "missing plugin",
    ];
    ERROR_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Cap output to `max_lines` lines, appending a truncation note when trimmed.
fn budget(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    let elided = lines.len() - max_lines;
    out.push_str(&format!("\n... [{elided} more line(s) truncated]"));
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn listing_actions_use_fixed_commands() {
        assert_eq!(listing_command("functions"), Some("afl"));
        assert_eq!(listing_command("info"), Some("iI"));
        assert_eq!(listing_command("disasm"), None);
        assert_eq!(listing_command("decompile"), None);
    }

    #[test]
    fn a_function_reaches_r2_only_as_a_formatted_address() {
        assert_eq!(at_address("pdf", 0x13c0), "pdf @ 0x13c0");
        assert_eq!(at_address("pdg", 0x4011a0), "pdg @ 0x4011a0");
    }

    #[test]
    fn r2_args_order_and_flags() {
        let argv = r2_args("pdg @ 0x13c0", Path::new("/work/a.out"));
        assert_eq!(
            argv,
            vec![
                "-e".to_string(),
                "scr.color=0".to_string(),
                "-q".to_string(),
                "-c".to_string(),
                "pdg @ 0x13c0".to_string(),
                "-A".to_string(),
                "/work/a.out".to_string(),
            ]
        );
    }

    #[test]
    fn only_hex_or_decimal_is_an_address() {
        assert_eq!(parse_target("0x13c0"), Target::Address(0x13c0));
        assert_eq!(parse_target(" 0X004011A0 "), Target::Address(0x4011a0));
        assert_eq!(parse_target("5056"), Target::Address(5056));
        // Everything else is a name, looked up and never sent to r2 — above
        // all the r2 command syntax that can run a shell.
        for name in [
            "main",
            "leaf(int*)",
            "main; !touch INJECTED_MARKER",
            "`!touch BACKTICK_MARKER`",
            "std::vector<int>::push_back",
            "main~call",
            "operator|",
            "0x13c0; !id",
            "0x",
            "13c0",
            "+10",
        ] {
            assert_eq!(
                parse_target(name),
                Target::Name(name.trim().to_string()),
                "{name}"
            );
        }
    }

    /// Names exactly as r2 5.9.8's `aflj` lists them for a C++ fixture.
    fn listing() -> Vec<R2Function> {
        [
            (0x1030, "sym.imp.printf"),
            (0x1110, "entry0"),
            (0x13c0, "dbg.leaf(int*)"),
            (0x13f0, "dbg.mid(int*)"),
            (0x1242, "dbg.ns::overload(int)"),
            (0x1273, "dbg.ns::overload(double)"),
            (0x146d, "dbg.main"),
            (0x11f9, "dbg.ns::operator+(ns::Vec2, ns::Vec2)"),
            (0x165a, "dbg.std::_Vector_base<int, std::allocator<int> >::~_Vector_base()"),
            (0x16a0, "dbg.void std::vector<int, std::allocator<int> >::_M_realloc_insert<int>(__gnu_cxx::__normal_iterator<int*, std::vector<int, std::allocator<int> > >, int&&)"),
            (0x2000, "fcn.00002000"),
        ]
        .into_iter()
        .map(|(addr, name)| R2Function {
            name: name.to_string(),
            addr,
        })
        .collect()
    }

    fn found(query: &str) -> Option<u64> {
        let functions = listing();
        match resolve(query, &functions) {
            Resolution::Found(f) => Some(f.addr),
            _ => None,
        }
    }

    #[test]
    fn resolves_listed_prefixed_bare_and_demangled_names() {
        // As listed by action "functions", and without r2's prefix.
        assert_eq!(found("dbg.leaf(int*)"), Some(0x13c0));
        assert_eq!(found("leaf(int*)"), Some(0x13c0));
        // Whitespace-insensitive, as people and demanglers disagree on it.
        assert_eq!(found("leaf(int *)"), Some(0x13c0));
        // A bare name, which r2 itself rejected (`Invalid tmpseek address`).
        assert_eq!(found("leaf"), Some(0x13c0));
        assert_eq!(found("main"), Some(0x146d));
        assert_eq!(found("printf"), Some(0x1030));
        assert_eq!(found("sym.imp.printf"), Some(0x1030));
        assert_eq!(found("entry0"), Some(0x1110));
        assert_eq!(found("fcn.00002000"), Some(0x2000));
        assert_eq!(found("ns::operator+"), Some(0x11f9));
        assert_eq!(
            found("std::_Vector_base<int, std::allocator<int>>::~_Vector_base"),
            Some(0x165a)
        );
    }

    #[test]
    fn a_bare_name_with_overloads_is_ambiguous_not_guessed() {
        let functions = listing();
        let Resolution::Ambiguous(hits) = resolve("ns::overload", &functions) else {
            panic!("two overloads must not resolve to one");
        };
        let addrs: Vec<u64> = hits.iter().map(|f| f.addr).collect();
        assert_eq!(addrs, vec![0x1242, 0x1273]);
    }

    #[test]
    fn a_wrong_signature_never_falls_back_to_the_bare_name() {
        // Only one `leaf` exists; asking for leaf(long) must not silently
        // disassemble leaf(int*).
        let functions = listing();
        assert!(matches!(
            resolve("leaf(long)", &functions),
            Resolution::Missing(_)
        ));
    }

    #[test]
    fn a_missing_name_offers_close_matches() {
        let functions = listing();
        let Resolution::Missing(close) = resolve("std::vector<int>::_M_realloc_insert", &functions)
        else {
            panic!("a template spelled differently is not an exact match");
        };
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].addr, 0x16a0);
        assert!(matches!(
            resolve("main~call", &functions),
            Resolution::Missing(close) if close.is_empty()
        ));
    }

    #[test]
    fn parses_aflj_with_offset_or_addr_keys() {
        let json =
            r#"[{"name":"dbg.main","offset":5229,"size":492},{"name":"entry0","addr":4368}]"#;
        assert_eq!(
            parse_function_list(json),
            Some(vec![
                R2Function {
                    name: "dbg.main".to_string(),
                    addr: 5229
                },
                R2Function {
                    name: "entry0".to_string(),
                    addr: 4368
                },
            ])
        );
        assert_eq!(parse_function_list("ERROR: Cannot open 'x'"), None);
        assert_eq!(parse_function_list(""), None);
    }

    #[test]
    fn without_params_keeps_operator_call_and_strips_qualifiers() {
        assert_eq!(without_params("ns::f(int) const"), "ns::f");
        assert_eq!(
            without_params("S::operator()(int) const &"),
            "S::operator()"
        );
        assert_eq!(without_params("main"), "main");
        // A trailing `)` that is not a parameter list stays put.
        assert_eq!(
            without_params("(anonymous namespace)::x"),
            "(anonymous namespace)::x"
        );
    }

    #[test]
    fn pdg_fallback_decision_on_missing_plugin() {
        // Empty output => plugin absent => fall back.
        assert!(pdg_unavailable(""));
        assert!(pdg_unavailable("   \n  "));
        // r2's "unknown command" style errors => fall back.
        assert!(pdg_unavailable("pdg: no such command"));
        assert!(pdg_unavailable("Unknown command 'pdg'"));
        assert!(pdg_unavailable("Cannot find function at 0x0"));
        // What r2 5.9 actually prints without r2ghidra. Unless it is matched too,
        // `decompile` shows the error and never falls back.
        assert!(pdg_unavailable(
            "INFO: Analyze all flags starting with sym. and entry0 (aa)
             ERROR: Missing plugin. Run: r2pm -ci r2ghidra"
        ));
    }

    #[test]
    fn pdg_available_when_pseudocode_present() {
        let pseudocode = "undefined8 main(void)\n{\n    return 0;\n}";
        assert!(!pdg_unavailable(pseudocode));
    }

    #[test]
    fn action_defaults_to_decompile() {
        assert_eq!(action_or_default(None), "decompile");
        assert_eq!(action_or_default(Some("")), "decompile");
        assert_eq!(action_or_default(Some("  ")), "decompile");
        assert_eq!(action_or_default(Some("disasm")), "disasm");
    }

    #[test]
    fn validates_known_actions() {
        for action in ["decompile", "disasm", "functions", "info"] {
            assert!(is_valid_action(action));
        }
        assert!(!is_valid_action("ghidra"));
        assert!(!is_valid_action(""));
    }

    #[test]
    fn function_defaults_to_main() {
        assert_eq!(function_or_default(None), "main");
        assert_eq!(function_or_default(Some("")), "main");
        assert_eq!(function_or_default(Some("  ")), "main");
        assert_eq!(function_or_default(Some("sym.foo")), "sym.foo");
        // Surrounding whitespace is trimmed.
        assert_eq!(function_or_default(Some("  0x1000 ")), "0x1000");
    }

    #[test]
    fn budget_passes_short_output_through() {
        let text = "line1\nline2\nline3";
        assert_eq!(budget(text, MAX_LINES), text);
    }

    #[test]
    fn budget_truncates_long_output_with_note() {
        let text = (0..300)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = budget(&text, MAX_LINES);
        let lines: Vec<&str> = out.lines().collect();
        // 250 content lines + 1 truncation note line.
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[0], "line0");
        assert_eq!(lines[MAX_LINES - 1], "line249");
        assert_eq!(lines[MAX_LINES], "... [50 more line(s) truncated]");
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "a.out"), PathBuf::from("/work/a.out"));
        assert_eq!(
            resolve_path(cwd, "bin/a.out"),
            PathBuf::from("/work/bin/a.out")
        );
        assert_eq!(resolve_path(cwd, "/abs/a.out"), PathBuf::from("/abs/a.out"));
    }

    // The r2 spawn-error mapping moved into `run::spawn_error` when this tool
    // was routed through `run_capped`; its package hint ("radare2") is
    // covered by `run::tests::missing_tool_names_its_package_when_it_differs`.

    #[test]
    fn combine_output_merges_stdout_and_stderr() {
        assert_eq!(combine_output(b"asm", b""), "asm");
        assert_eq!(combine_output(b"asm", b"warn"), "asm\nwarn");
        assert_eq!(combine_output(b"asm\n", b"warn"), "asm\nwarn");
        assert_eq!(combine_output(b"", b"err"), "err");
    }

    #[test]
    fn a_misnamed_argument_is_refused_not_ignored() {
        let err = serde_json::from_str::<DecompileArgs>(r#"{"binary":"a.out","symbol":"main"}"#)
            .err()
            .expect("an unknown field must be an error");
        assert!(err.to_string().contains("unknown field `symbol`"), "{err}");
    }

    // ---- real radare2 ------------------------------------------------------

    use crate::program_args::fixture::have;

    const FIXTURE_CPP: &str = r#"#include <cstdio>
namespace ns {
__attribute__((noinline)) int overload(int x) { return x + 1; }
__attribute__((noinline)) int overload(double x) { return (int)x + 2; }
}
__attribute__((noinline)) int leaf(int *p) { return *p; }
int main(int argc, char **argv) {
  int v = argc;
  std::printf("%d %s\n", leaf(&v) + ns::overload(1) + ns::overload(2.0), argv[0]);
  return 0;
}
"#;

    /// A fresh directory holding `prog` built from [`FIXTURE_CPP`], or `None`
    /// (the caller skips) when r2 or a C++ compiler is missing.
    fn r2_fixture(tag: &str) -> Option<PathBuf> {
        if !have("r2") || !have("c++") {
            eprintln!("skip: needs r2 and c++");
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "byteask_decompile_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prog.cpp"), FIXTURE_CPP).unwrap();
        let built = std::process::Command::new("c++")
            .args(["-g", "-O0", "-o", "prog", "prog.cpp"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !built {
            eprintln!("skip: could not compile the fixture");
            return None;
        }
        Some(dir)
    }

    fn call(action: &str, function: &str) -> DecompileArgs {
        DecompileArgs {
            binary: "prog".to_string(),
            action: Some(action.to_string()),
            function: Some(function.to_string()),
        }
    }

    #[tokio::test]
    async fn real_r2_never_runs_the_callers_text_as_commands() {
        // Measured before the fix: `main; !touch X` ran `touch`, a
        // backtick ran it via substitution, and a C++ template name's `>`
        // redirected r2's output into a new file named `::push_back`.
        let Some(dir) = r2_fixture("inject") else {
            return;
        };
        let tool = DecompileTool { cwd: dir.clone() };
        let attacks = [
            ("disasm", "main; !touch INJECTED_MARKER"),
            ("disasm", "`!touch BACKTICK_MARKER`"),
            ("disasm", "std::vector<int>::push_back"),
            ("disasm", "main > REDIRECT_MARKER"),
            ("disasm", "main | touch PIPE_MARKER"),
            ("decompile", "main;!touch DECOMPILE_MARKER"),
        ];
        for (action, function) in attacks {
            let out = tool.run(&call(action, function)).await.expect("run");
            assert!(
                out.contains("No function named") || out.contains("matches"),
                "{function:?} must be looked up, not executed: {out}"
            );
        }
        for marker in [
            "INJECTED_MARKER",
            "BACKTICK_MARKER",
            "::push_back",
            "REDIRECT_MARKER",
            "PIPE_MARKER",
            "DECOMPILE_MARKER",
        ] {
            assert!(
                !dir.join(marker).exists(),
                "r2 executed caller text: {marker} exists"
            );
        }
        // `~` is r2's grep: it used to filter the output down to `call` lines.
        let grep = tool.run(&call("disasm", "main~call")).await.expect("run");
        assert!(grep.contains("No function named \"main~call\""), "{grep}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn real_r2_disassembles_bare_demangled_and_address_targets() {
        // Given a raw name, r2 answers `Invalid tmpseek address` for every C++ name,
        // including the `dbg.leaf(int*)` its own function list prints.
        let Some(dir) = r2_fixture("names") else {
            return;
        };
        let tool = DecompileTool { cwd: dir.clone() };
        for function in ["leaf", "leaf(int*)", "leaf(int *)"] {
            let out = tool.run(&call("disasm", function)).await.expect("run");
            let header = out.lines().next().unwrap_or_default();
            assert!(
                header.starts_with("function: ") && header.contains("leaf"),
                "{function}: {out}"
            );
            assert!(!out.contains("Invalid tmpseek"), "{function}: {out}");
            assert!(!out.contains('\u{1b}'), "no ANSI escapes: {out}");
        }
        let out = tool
            .run(&call("disasm", "ns::overload"))
            .await
            .expect("run");
        assert!(out.contains("matches 2 functions"), "{out}");
        assert!(
            out.contains("ns::overload(int)") && out.contains("ns::overload(double)"),
            "{out}"
        );
        // An address is used as given, with no lookup.
        let addr = out
            .lines()
            .find(|line| line.contains("ns::overload(int)"))
            .and_then(|line| line.split_whitespace().next())
            .expect("an address line")
            .to_string();
        let by_addr = tool.run(&call("disasm", &addr)).await.expect("run");
        assert!(by_addr.contains("ns::overload(int)"), "{by_addr}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn real_r2_decompile_falls_back_to_disassembly_without_r2ghidra() {
        // Without r2ghidra, `decompile` must not print r2's `Missing plugin`
        // error in place of the disassembly it promises.
        let Some(dir) = r2_fixture("fallback") else {
            return;
        };
        let tool = DecompileTool { cwd: dir.clone() };
        let out = tool.run(&call("decompile", "main")).await.expect("run");
        if tool.pdg_installed().await.expect("probe") {
            assert!(!out.contains("Missing plugin"), "{out}");
        } else {
            assert!(out.contains(FALLBACK_NOTE), "{out}");
            assert!(
                out.contains("main") && out.lines().count() > 5,
                "must show main's disassembly: {out}"
            );
            assert!(!out.contains("Missing plugin"), "{out}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
