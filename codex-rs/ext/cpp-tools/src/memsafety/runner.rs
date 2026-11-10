//! Run a built binary on one input and classify the outcome, for the fix
//! verifier. "Crashed" means a sanitizer report OR a fatal signal; a clean
//! exit (any code) is not a crash — many CLIs reject bad input with a non-zero
//! status, which is correct behaviour, not a memory bug.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_extension_api::FunctionCallError;

use crate::run::RunOutcome;
use crate::run::run_capped_env_stdin;

use super::evidence;
use super::evidence::Evidence;

/// How the input reaches the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputMode {
    /// The input file's path is the last argument (libFuzzer-style, and most CLIs).
    Arg,
    /// The input file is the program's stdin.
    Stdin,
}

/// What one run said.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    /// A sanitizer report or a fatal signal.
    Crashed {
        signal: bool,
        evidence: Option<Evidence>,
        /// The bug class, for a one-line summary.
        bug: String,
    },
    /// Exited without a sanitizer report (any exit code).
    Clean {
        code: Option<i32>,
        stdout: String,
        /// Truncated stdout ⇒ the behaviour diff cannot trust an equality.
        truncated: bool,
    },
    TimedOut {
        secs: u64,
    },
}

impl Outcome {
    pub(crate) fn crashed(&self) -> bool {
        matches!(self, Outcome::Crashed { .. })
    }
}

/// Run `binary run_args…` on `input` under the sanitizer runtime options in
/// `env`, and classify it. `{poc}` in `run_args` is replaced by the input
/// path; otherwise the path is appended (Arg) or piped (Stdin).
pub(crate) async fn run_once(
    binary: &Path,
    run_args: &[String],
    input: &Path,
    mode: InputMode,
    env: &[(String, String)],
    cwd: &Path,
    timeout: Duration,
) -> Result<Outcome, FunctionCallError> {
    let mut args: Vec<String> = Vec::new();
    let mut substituted = false;
    for arg in run_args {
        if arg.contains("{poc}") {
            args.push(arg.replace("{poc}", &input.display().to_string()));
            substituted = true;
        } else {
            args.push(arg.clone());
        }
    }
    let stdin = match mode {
        InputMode::Stdin => Some(input),
        InputMode::Arg => {
            if !substituted {
                args.push(input.display().to_string());
            }
            None
        }
    };

    let program = binary.display().to_string();
    let run = run_capped_env_stdin(&program, &args, cwd, timeout, env, stdin).await?;
    Ok(classify(run))
}

fn classify(run: RunOutcome) -> Outcome {
    match run {
        RunOutcome::TimedOut { secs } => Outcome::TimedOut { secs },
        RunOutcome::Finished {
            success,
            code,
            stdout,
            stderr,
            truncated,
        } => {
            if let Some(ev) = evidence::parse(&stderr) {
                let bug = ev.bug.clone();
                return Outcome::Crashed {
                    signal: false,
                    evidence: Some(ev),
                    bug,
                };
            }
            // A fatal signal with no captured report (e.g. a bare SIGSEGV, or a
            // report cut by truncation) is still a crash.
            if !success && code.is_none() {
                return Outcome::Crashed {
                    signal: true,
                    evidence: None,
                    bug: "killed by signal (no sanitizer report captured)".to_string(),
                };
            }
            Outcome::Clean {
                code,
                stdout,
                truncated,
            }
        }
    }
}

/// Write `bytes` to a fresh temp file and return its path. The caller removes it.
pub(crate) fn write_temp(prefix: &str, bytes: &[u8]) -> Result<PathBuf, FunctionCallError> {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        fastrand_hex()
    ));
    std::fs::write(&path, bytes).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "cannot write temp input {}: {err}",
            path.display()
        ))
    })?;
    Ok(path)
}

fn fastrand_hex() -> String {
    use std::time::SystemTime;
    let n = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(success: bool, code: Option<i32>, stderr: &str) -> RunOutcome {
        RunOutcome::Finished {
            success,
            code,
            stdout: "out".to_string(),
            stderr: stderr.to_string(),
            truncated: false,
        }
    }

    #[test]
    fn a_sanitizer_report_is_a_crash() {
        let stderr = "==1==ERROR: AddressSanitizer: heap-use-after-free on address 0x1\n    #0 0x2 in f /a.c:1\n";
        let outcome = classify(finished(false, None, stderr));
        match outcome {
            Outcome::Crashed {
                signal,
                evidence,
                bug,
            } => {
                assert!(!signal);
                assert!(evidence.is_some());
                assert_eq!(bug, "heap-use-after-free");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bare_signal_is_a_crash() {
        assert!(classify(finished(false, None, "")).crashed());
    }

    #[test]
    fn a_nonzero_exit_without_a_report_is_clean() {
        // A CLI rejecting bad input with exit 1 is correct, not a crash.
        let outcome = classify(finished(false, Some(1), "error: bad file\n"));
        assert!(!outcome.crashed());
        match outcome {
            Outcome::Clean { code, .. } => assert_eq!(code, Some(1)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exit_zero_is_clean() {
        assert!(!classify(finished(true, Some(0), "")).crashed());
    }
}
