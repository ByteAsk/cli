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

const TOOL_NAME: &str = "cpp_symbolize";
/// readelf / addr2line answer in seconds; this only bounds a hang.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct SymbolizeTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolizeArgs {
    binary: String,
    address: String,
    #[serde(default)]
    load_base: Option<String>,
    /// How to treat the addresses w.r.t. the return-address off-by-one.
    /// A return address points at the instruction AFTER the call, so addr2line
    /// must be given `addr - 1` to land on the calling line. None (default)
    /// applies the gdb backtrace convention: the FIRST address is the leaf /
    /// fault PC (exact) and every later address is a return address (`- 1`).
    /// `true` = every address is a return address; `false` = adjust none.
    #[serde(default)]
    return_addresses: Option<bool>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for SymbolizeTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "binary".to_string(),
            JsonSchema::string(Some(
                "Path to the ELF binary (executable or shared object), relative to the \
                 workspace or absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "address".to_string(),
            JsonSchema::string(Some(
                "A single runtime crash/backtrace address in hex (e.g. 0x55a3...c4), or a \
                 comma/space-separated list of such addresses."
                    .to_string(),
            )),
        );
        properties.insert(
            "load_base".to_string(),
            JsonSchema::string(Some(
                "Optional hex module load base from /proc/<pid>/maps. For a PIE binary the \
                 file-relative address is computed as runtime_addr - load_base. The load base \
                 is the start of the module's FIRST mapping (the line whose file offset is \
                 00000000), not the start of its r-xp (code) line; the r-xp start gives a \
                 confidently wrong function."
                    .to_string(),
            )),
        );
        properties.insert(
            "return_addresses".to_string(),
            JsonSchema::boolean(Some(
                "How to handle the return-address off-by-one. A return address points just \
                 AFTER its call, so addr2line needs `addr - 1` to name the calling line. Omit \
                 (default) for the gdb-backtrace convention: the first address is the leaf/fault \
                 PC (exact) and later ones are return addresses (-1). Set true when every address \
                 is a return address, false to disable the adjustment."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Symbolize a crash/backtrace runtime address to function + file:line \
                using addr2line, correctly handling the PIE load-base offset \
                (runtime_addr - load_base) that is easy to get wrong. C/C++ binaries only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["binary".to_string(), "address".to_string()]),
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

impl SymbolizeTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: SymbolizeArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let binary = resolve_path(&self.cwd, &args.binary);
        if !binary.exists() {
            return Err(FunctionCallError::RespondToModel(format!(
                "binary not found: {}",
                binary.display()
            )));
        }
        // A regular file only: a FIFO would block readelf forever, and a
        // directory would be "symbolized".
        if !binary.is_file() {
            return Err(FunctionCallError::RespondToModel(format!(
                "not a regular file: {} (pass the ELF executable or shared library the \
                 addresses came from).",
                binary.display()
            )));
        }

        // Parse the requested runtime addresses.
        let runtime_addrs = parse_addresses(&args.address)
            .map_err(FunctionCallError::RespondToModel)?;
        if runtime_addrs.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "no addresses provided".to_string(),
            ));
        }

        // Parse the optional load base.
        let load_base = match args.load_base.as_deref() {
            Some(s) => Some(parse_hex(s).map_err(FunctionCallError::RespondToModel)?),
            None => None,
        };

        // Detect whether the ELF is position-independent.
        let is_pie = detect_pie(&binary).await?;

        // Decide the offset rule and compute file-relative addresses.
        let apply_offset = is_pie && load_base.is_some();
        let mut file_addrs = Vec::with_capacity(runtime_addrs.len());
        let mut any_return_adj = false;
        for (i, &addr) in runtime_addrs.iter().enumerate() {
            let file_addr = compute_file_addr(addr, load_base, is_pie)
                .map_err(FunctionCallError::RespondToModel)?;
            // Return-address off-by-one. A return address lands just past its
            // call, so `-1` is needed to resolve the calling source line.
            let is_return = is_return_frame(i, args.return_addresses);
            let adjusted = if is_return {
                file_addr.saturating_sub(1)
            } else {
                file_addr
            };
            // Only claim an adjustment when one actually happened — a 0 address
            // saturates and the note would otherwise describe a no-op.
            if adjusted != file_addr {
                any_return_adj = true;
            }
            file_addrs.push(adjusted);
        }

        let mut note = offset_note(is_pie, load_base, apply_offset);
        if any_return_adj {
            note.push_str(
                "\nnote: subtracted 1 from return-address frames (a return address points after \
                 its call) so addr2line resolves the calling line; pass return_addresses=false to \
                 disable.",
            );
        }

        // Run addr2line on the file-relative addresses — through run_capped
        // (a wall-clock cap; a spawn failure is not `Fatal`).
        let mut argv: Vec<String> = ["-e", "", "-f", "-C", "-i", "-p"]
            .iter()
            .map(ToString::to_string)
            .collect();
        argv[1] = binary.display().to_string();
        for addr in &file_addrs {
            argv.push(format!("0x{addr:x}"));
        }
        let (stdout, stderr) =
            match run_capped("addr2line", &argv, &self.cwd, TOOL_TIMEOUT).await? {
                RunOutcome::Finished { stdout, stderr, .. } => (stdout, stderr),
                RunOutcome::TimedOut { secs } => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "addr2line did not finish within {secs}s and was stopped; nothing was \
                         resolved."
                    )));
                }
            };

        let mut text = String::new();
        text.push_str(&note);
        text.push('\n');
        if !stdout.trim().is_empty() {
            text.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !stdout.trim().is_empty() {
                text.push('\n');
            }
            text.push_str("stderr: ");
            text.push_str(stderr.trim_end());
        }

        let result = if text.contains("??:") {
            check_result::CheckResult::approximate(
                TOOL_NAME,
                "Some addresses had no line information (shown as ??), so only part of the trace resolved.",
            )
        } else {
            check_result::CheckResult::pass(TOOL_NAME, "Every address resolved to a source line.")
        };
        Ok(crate::report::report(TOOL_NAME, &args.binary, text, result))
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

/// Should frame `index` be treated as a return address (`addr - 1`)?
/// `arg` = the explicit `return_addresses` override; None applies the gdb
/// backtrace convention (leaf frame exact, callers are return addresses).
fn is_return_frame(index: usize, arg: Option<bool>) -> bool {
    match arg {
        Some(v) => v,
        None => index > 0,
    }
}

/// Parse a single hex literal (with optional `0x`/`0X` prefix) into a `u64`.
fn parse_hex(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if digits.is_empty() {
        return Err(format!("not a hex address: {s:?}"));
    }
    u64::from_str_radix(digits, 16).map_err(|_| format!("not a hex address: {s:?}"))
}

/// Parse one or more hex addresses separated by commas and/or whitespace.
fn parse_addresses(input: &str) -> Result<Vec<u64>, String> {
    input
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|tok| !tok.is_empty())
        .map(parse_hex)
        .collect()
}

/// Compute the file-relative (link-time) address from a runtime address.
///
/// For a PIE binary with a known load base the runtime address is the load
/// base plus the file-relative address, so we subtract the load base. For a
/// non-PIE binary (or when no load base is known) the runtime address already
/// matches the file/link-time address and is used as-is.
fn compute_file_addr(runtime_addr: u64, load_base: Option<u64>, is_pie: bool) -> Result<u64, String> {
    match load_base {
        Some(base) if is_pie => runtime_addr.checked_sub(base).ok_or_else(|| {
            format!(
                "load base 0x{base:x} is greater than runtime address 0x{runtime_addr:x}; \
                 the address does not belong to this module's mapping"
            )
        }),
        _ => Ok(runtime_addr),
    }
}

/// One-line human-readable description of which offset rule was applied.
fn offset_note(is_pie: bool, load_base: Option<u64>, applied: bool) -> String {
    if applied {
        let base = load_base.unwrap_or(0);
        format!(
            "PIE: subtracted load base (file_addr = runtime_addr - 0x{base:x}); ELF Type=DYN."
        )
    } else if is_pie {
        "PIE binary (Type=DYN) but no load_base given: used addr as-is. If these are runtime \
         addresses from a running process, pass load_base: the start of the module's FIRST \
         mapping in /proc/<pid>/maps (file offset 00000000), not its r-xp line."
            .to_string()
    } else {
        "non-PIE: used addr as-is (ELF Type=EXEC, fixed load address).".to_string()
    }
}

/// Run `readelf -h <binary>` and return true when the ELF `Type:` line reports a
/// position-independent object (`DYN`).
///
/// A file that is not ELF at all (readelf fails, no `Type:` line) must not
/// read as "non-PIE ... ELF Type=EXEC" and be symbolized anyway — a confident
/// wrong premise. It is an error.
async fn detect_pie(binary: &Path) -> Result<bool, FunctionCallError> {
    let argv = vec!["-h".to_string(), binary.display().to_string()];
    let dir = binary.parent().unwrap_or(Path::new("."));
    let (success, stdout, stderr) = match run_capped("readelf", &argv, dir, TOOL_TIMEOUT).await? {
        RunOutcome::Finished {
            success,
            code,
            stdout,
            stderr,
            ..
        } => {
            // A crashed readelf says nothing about the file.
            if let Some(note) = crate::run::crash_note("readelf", success, code) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{note} Nothing was symbolized. stderr: {}",
                    stderr.trim()
                )));
            }
            (success, stdout, stderr)
        }
        RunOutcome::TimedOut { secs } => {
            return Err(FunctionCallError::RespondToModel(format!(
                "readelf did not finish within {secs}s and was stopped; nothing was resolved."
            )));
        }
    };
    elf_type_is_pie(success, &stdout).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{} is not an ELF file readelf can read, so its addresses cannot be symbolized \
             here (a Mach-O or PE binary needs atos / llvm-symbolizer on its own platform). \
             readelf said: {}",
            binary.display(),
            stderr.trim()
        ))
    })
}

/// `Some(is_pie)` from a successful `readelf -h` that printed an ELF `Type:`
/// line; `None` when it is not a readable ELF.
fn elf_type_is_pie(success: bool, readelf_header: &str) -> Option<bool> {
    let has_type = readelf_header
        .lines()
        .any(|line| line.trim_start().starts_with("Type:"));
    (success && has_type).then(|| parse_is_pie(readelf_header))
}

/// True when a `readelf -h` dump describes a `DYN` (position-independent) ELF.
fn parse_is_pie(readelf_header: &str) -> bool {
    readelf_header
        .lines()
        .find(|line| line.trim_start().starts_with("Type:"))
        .map(|line| line.contains("DYN"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn return_frame_default_is_gdb_convention() {
        // Default (None) — leaf frame #0 exact, callers are return addresses.
        assert!(!is_return_frame(0, None));
        assert!(is_return_frame(1, None));
        assert!(is_return_frame(5, None));
    }

    #[test]
    fn return_frame_explicit_override() {
        // Some(true) => every frame is a return address; Some(false) => none.
        assert!(is_return_frame(0, Some(true)));
        assert!(is_return_frame(3, Some(true)));
        assert!(!is_return_frame(0, Some(false)));
        assert!(!is_return_frame(3, Some(false)));
    }

    // ---- parse_hex --------------------------------------------------------

    #[test]
    fn parses_hex_with_and_without_prefix() {
        assert_eq!(parse_hex("0x10").unwrap(), 16);
        assert_eq!(parse_hex("0X1A").unwrap(), 26);
        assert_eq!(parse_hex("ff").unwrap(), 255);
        assert_eq!(parse_hex(" 0x55a3c4 ").unwrap(), 0x55a3c4);
    }

    #[test]
    fn parses_hex_uppercase_digits_with_and_without_prefix() {
        // Uppercase hex digits are valid with either prefix or bare.
        assert_eq!(parse_hex("0xDEADBEEF").unwrap(), 0xdead_beef);
        assert_eq!(parse_hex("0XCAFE").unwrap(), 0xcafe);
        assert_eq!(parse_hex("ABCDEF").unwrap(), 0xab_cdef);
    }

    #[test]
    fn parses_hex_strips_surrounding_whitespace() {
        // Leading/trailing spaces and tabs are trimmed before parsing.
        assert_eq!(parse_hex("\t0x1\t").unwrap(), 1);
        assert_eq!(parse_hex("  20  ").unwrap(), 0x20);
    }

    #[test]
    fn parses_hex_max_u64() {
        assert_eq!(parse_hex("0xffffffffffffffff").unwrap(), u64::MAX);
    }

    #[test]
    fn rejects_non_hex() {
        assert!(parse_hex("0x").is_err());
        assert!(parse_hex("zz").is_err());
        assert!(parse_hex("").is_err());
    }

    #[test]
    fn rejects_whitespace_only_and_bare_prefix() {
        // After trimming these reduce to empty digits.
        assert!(parse_hex("   ").is_err());
        assert!(parse_hex("0X").is_err());
        assert!(parse_hex(" 0x ").is_err());
    }

    #[test]
    fn rejects_overflowing_hex() {
        // 17 hex digits (2^64) is one nibble too wide for u64.
        assert!(parse_hex("0x10000000000000000").is_err());
    }

    #[test]
    fn hex_error_message_quotes_the_input() {
        let err = parse_hex("zz").unwrap_err();
        assert!(err.contains("not a hex address"));
        assert!(err.contains("\"zz\""));
    }

    // ---- parse_addresses --------------------------------------------------

    #[test]
    fn parses_single_address() {
        assert_eq!(parse_addresses("0x10").unwrap(), vec![16]);
        assert_eq!(parse_addresses("  0x10 ").unwrap(), vec![16]);
    }

    #[test]
    fn parses_comma_separated_addresses() {
        assert_eq!(parse_addresses("0x1,0x2,0x3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parses_space_separated_addresses() {
        assert_eq!(parse_addresses("0x1 0x2 0x3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parses_tab_separated_addresses() {
        assert_eq!(parse_addresses("0x1\t0x2\t0x3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parses_newline_separated_addresses() {
        assert_eq!(parse_addresses("0x1\n0x2\n0x3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parses_mixed_separators_and_blank_tokens() {
        // Commas, spaces, tabs and newlines may be freely mixed; empty tokens
        // produced by adjacent separators are dropped, not parsed as errors.
        assert_eq!(
            parse_addresses("0xa, 0xb,\t0xc\n0xd  0xe").unwrap(),
            vec![10, 11, 12, 13, 14]
        );
        assert_eq!(parse_addresses("0x1,,0x2").unwrap(), vec![1, 2]);
    }

    #[test]
    fn parse_addresses_empty_input_yields_empty_vec() {
        // No tokens at all → empty list (handle_call turns this into an error).
        assert_eq!(parse_addresses("").unwrap(), Vec::<u64>::new());
        assert_eq!(parse_addresses("  \t\n ").unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn parse_addresses_propagates_invalid_token() {
        let err = parse_addresses("0x1, zz, 0x3").unwrap_err();
        assert!(err.contains("not a hex address"));
    }

    // ---- A file that is not an ELF binary ------------------------------------

    #[test]
    fn only_a_readable_elf_header_decides_pie() {
        let header = "ELF Header:\n  Type:                              DYN (Position-Independent Executable file)\n";
        assert_eq!(elf_type_is_pie(true, header), Some(true));
        assert_eq!(elf_type_is_pie(true, &header.replace("DYN", "EXEC")), Some(false));
        // readelf failed, or printed no Type: line -> not an ELF we can use.
        assert_eq!(elf_type_is_pie(false, header), None);
        assert_eq!(elf_type_is_pie(false, ""), None);
    }

    #[tokio::test]
    async fn a_non_elf_file_is_refused_not_called_exec() {
        // A text file / directory must not read as "non-PIE ... ELF Type=EXEC".
        if !crate::program_args::fixture::have("readelf") {
            eprintln!("skip: readelf not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a binary").unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        let tool = SymbolizeTool {
            cwd: dir.path().to_path_buf(),
        };
        for binary in ["notes.txt", "adir"] {
            let err = crate::test_support::call(
                &tool,
                serde_json::json!({"binary": binary, "address": "0x1139"}),
            )
            .await
            .expect_err(binary);
            let FunctionCallError::RespondToModel(message) = err else {
                panic!("{binary}: expected a model-facing error, got {err:?}");
            };
            assert!(!message.contains("EXEC"), "{binary}: {message}");
        }
    }

    // ---- parse_is_pie / readelf detection ---------------------------------

    #[test]
    fn detects_pie_from_readelf_dyn_type() {
        let header = "\
ELF Header:
  Magic:   7f 45 4c 46
  Class:                             ELF64
  Type:                              DYN (Position-Independent Executable file)
  Machine:                           Advanced Micro Devices X86-64";
        assert!(parse_is_pie(header));
    }

    #[test]
    fn detects_non_pie_from_readelf_exec_type() {
        let header = "\
ELF Header:
  Class:                             ELF64
  Type:                              EXEC (Executable file)
  Machine:                           Advanced Micro Devices X86-64";
        assert!(!parse_is_pie(header));
    }

    #[test]
    fn detects_non_pie_when_type_line_missing() {
        // A truncated/garbage dump with no `Type:` line is treated as non-PIE.
        let header = "\
ELF Header:
  Magic:   7f 45 4c 46
  Class:                             ELF64
  Machine:                           Advanced Micro Devices X86-64";
        assert!(!parse_is_pie(header));
    }

    #[test]
    fn parse_is_pie_handles_empty_dump() {
        assert!(!parse_is_pie(""));
    }

    #[test]
    fn parse_is_pie_matches_dyn_only_on_the_type_line() {
        // A `DYN` mention elsewhere (e.g. a comment-ish line) must not trip
        // detection — only the `Type:` line is consulted.
        let header = "\
ELF Header:
  Note: built as DYN-capable toolchain
  Type:                              EXEC (Executable file)";
        assert!(!parse_is_pie(header));
    }

    // ---- compute_file_addr (offset math) ----------------------------------

    #[test]
    fn pie_with_load_base_subtracts_offset() {
        // Classic PIE case: runtime addr from maps minus the module load base
        // yields the link-time file address that addr2line understands.
        let runtime = 0x5555_5555_91c4u64;
        let base = 0x5555_5555_5000u64;
        let file_addr = compute_file_addr(runtime, Some(base), true).unwrap();
        assert_eq!(file_addr, 0x41c4);
    }

    #[test]
    fn pie_with_equal_load_base_yields_zero() {
        // runtime == base is the module's first byte → file offset 0, not an error.
        assert_eq!(compute_file_addr(0x1000, Some(0x1000), true).unwrap(), 0);
    }

    #[test]
    fn pie_without_load_base_uses_addr_as_is() {
        let addr = 0x11a0u64;
        assert_eq!(compute_file_addr(addr, None, true).unwrap(), addr);
    }

    #[test]
    fn non_pie_ignores_load_base_and_uses_addr_as_is() {
        // A fixed-load EXEC: the runtime address already is the file address,
        // even if a (spurious) load base is supplied.
        let addr = 0x40_11a0u64;
        assert_eq!(compute_file_addr(addr, Some(0x1000), false).unwrap(), addr);
        assert_eq!(compute_file_addr(addr, None, false).unwrap(), addr);
    }

    #[test]
    fn load_base_greater_than_addr_is_an_error() {
        let err = compute_file_addr(0x1000, Some(0x2000), true).unwrap_err();
        assert!(err.contains("does not belong"));
        // The error reports both offending values in hex.
        assert!(err.contains("0x2000"));
        assert!(err.contains("0x1000"));
    }

    #[test]
    fn non_pie_with_load_base_greater_than_addr_does_not_underflow() {
        // The underflow guard only applies to PIE; non-PIE always uses addr as-is.
        assert_eq!(compute_file_addr(0x1000, Some(0x2000), false).unwrap(), 0x1000);
    }

    // ---- offset_note ------------------------------------------------------

    #[test]
    fn offset_note_applied_pie_mentions_subtraction_and_base() {
        let note = offset_note(true, Some(0x1000), true);
        assert!(note.contains("subtracted load base"));
        assert!(note.contains("0x1000"));
        assert!(note.contains("Type=DYN"));
    }

    #[test]
    fn offset_note_pie_without_base_advises_proc_maps() {
        let note = offset_note(true, None, false);
        assert!(note.contains("no load_base"));
        assert!(note.contains("used addr as-is"));
        assert!(note.contains("/proc/<pid>/maps"));
    }

    #[test]
    fn offset_note_non_pie_mentions_exec_fixed_load() {
        let note = offset_note(false, None, false);
        assert!(note.contains("non-PIE"));
        assert!(note.contains("Type=EXEC"));
        assert!(note.contains("used addr as-is"));
    }

    // ---- resolve_path -----------------------------------------------------

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_path(cwd, "build/app"),
            PathBuf::from("/work/build/app")
        );
        assert_eq!(resolve_path(cwd, "/abs/app"), PathBuf::from("/abs/app"));
    }
}
