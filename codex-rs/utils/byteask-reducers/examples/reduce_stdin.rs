//! Evaluation helper: read raw tool output on stdin, write what the model
//! would receive on stdout. Used by replay scripts to COUNT tokens before and
//! after with the real tokenizer.
//!
//!   reduce_stdin                         the two terminal transforms only (default policy)
//!   reduce_stdin --command "cargo test"  the full family pipeline for that command
//!   reduce_stdin --command "…" --all     ...with every dark reducer switched on
//!
//! The last line of stderr names the family and what fired, so a caller can assert on it.
use std::io::Read;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args
        .iter()
        .position(|a| a == "--command")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let all = args.iter().any(|a| a == "--all");

    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() {
        std::process::exit(2);
    }
    let text = String::from_utf8_lossy(&raw);
    let mut policy = byteask_output_reducers::Policy::default();
    if all {
        for name in byteask_output_reducers::policy::DARK_REDUCERS {
            policy.enabled.insert((*name).to_string());
        }
    }
    let exists =
        |p: &str| std::path::Path::new(p).exists() || !std::path::Path::new(p).is_absolute();
    let ctx = byteask_output_reducers::Ctx {
        command: command.as_deref(),
        path_exists: Some(&exists),
    };
    let out = byteask_output_reducers::reduce(&text, &ctx, &policy);
    let mut body = out.text;
    if !out.fired.is_empty() {
        body.push_str(&format!("\n[byteask: {}]", out.fired.join(" ")));
    }
    let _ = std::io::stdout().write_all(body.as_bytes());
    eprintln!(
        "family={} fired={} lossy={}",
        out.family.name(),
        out.fired.join(","),
        out.lossy
    );
}
