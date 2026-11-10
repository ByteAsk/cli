//! `byteask-replay`: the replay library from the command line, JSON in and JSON out.
//!
//! ```text
//! byteask-replay compare [--rules RULES.json] [--lines] A B
//! byteask-replay run SPEC.json [--workdir DIR]
//! ```
//! Prints one JSON report on stdout. Exit status: 0 same, 1 different, 2 could not compare.

use std::path::PathBuf;
use std::process::ExitCode;

use byteask_replay::Rules;
use byteask_replay::compare_streams;
use byteask_replay::report;
use byteask_replay::run::Outcome;
use byteask_replay::run::OutputFormat;
use byteask_replay::run::ReplaySpec;
use byteask_replay::run::read_output;
use byteask_replay::run::replay_and_compare;
use serde_json::json;

const USAGE: &str = "usage:\n  byteask-replay compare [--rules RULES.json] [--lines] A B\n  byteask-replay run SPEC.json [--workdir DIR]\nPrints a JSON report. Exit status: 0 same, 1 different, 2 could not compare.";

fn emit(value: &serde_json::Value, outcome: Outcome) -> ExitCode {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
    ExitCode::from(match outcome {
        Outcome::Same => 0,
        Outcome::Different => 1,
        Outcome::NotRun => 2,
    })
}

fn not_run(reason: String) -> ExitCode {
    emit(
        &json!({"outcome": "not_run", "reason": reason, "sentence": format!("NOT RUN: {reason}")}),
        Outcome::NotRun,
    )
}

fn compare(args: &[String]) -> ExitCode {
    let mut rules_path = None;
    let mut format = OutputFormat::Jsonl;
    let mut files = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rules" => match it.next() {
                Some(p) => rules_path = Some(PathBuf::from(p)),
                None => return not_run("--rules needs a file".to_string()),
            },
            "--lines" => format = OutputFormat::Lines,
            other if other.starts_with("--") => {
                return not_run(format!("unknown option {other}\n{USAGE}"));
            }
            other => files.push(PathBuf::from(other)),
        }
    }
    let [a, b] = files.as_slice() else {
        return not_run(format!("compare needs exactly two files\n{USAGE}"));
    };
    let rules = match rules_path {
        None => Rules::exact(),
        Some(p) => match std::fs::read_to_string(&p) {
            Err(e) => return not_run(format!("cannot read {}: {e}", p.display())),
            Ok(text) => match Rules::from_json(&text) {
                Ok(r) => r,
                Err(e) => return not_run(e.0),
            },
        },
    };
    let streams = read_output(a, format).and_then(|sa| read_output(b, format).map(|sb| (sa, sb)));
    let (sa, sb) = match streams {
        Ok(s) => s,
        Err(e) => return not_run(format!("cannot open an input file: {e}")),
    };
    match compare_streams(sa, sb, &rules) {
        Err(e) => not_run(e.to_string()),
        Ok(c) => {
            let outcome = if c.same {
                Outcome::Same
            } else {
                Outcome::Different
            };
            let sentence = report::sentence(&c, "A", "B");
            emit(
                &json!({"outcome": outcome, "sentence": sentence, "comparison": c}),
                outcome,
            )
        }
    }
}

fn run(args: &[String]) -> ExitCode {
    let mut spec_path = None;
    let mut workdir = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--workdir" => match it.next() {
                Some(p) => workdir = Some(PathBuf::from(p)),
                None => return not_run("--workdir needs a directory".to_string()),
            },
            other if other.starts_with("--") => {
                return not_run(format!("unknown option {other}\n{USAGE}"));
            }
            other => spec_path = Some(PathBuf::from(other)),
        }
    }
    let Some(spec_path) = spec_path else {
        return not_run(format!("run needs a spec file\n{USAGE}"));
    };
    let spec: ReplaySpec = match std::fs::read_to_string(&spec_path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => return not_run(format!("spec {}: {e}", spec_path.display())),
    };
    let workdir = workdir.unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("byteask-replay-{}-{nanos}", std::process::id()))
    });
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        return not_run(format!("cannot create {}: {e}", workdir.display()));
    }
    let report = replay_and_compare(&spec, &workdir);
    let value = serde_json::to_value(&report).unwrap_or_else(|e| json!({"error": e.to_string()}));
    emit(&value, report.outcome)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("compare") => compare(&args[1..]),
        Some("run") => run(&args[1..]),
        _ => not_run(USAGE.to_string()),
    }
}
