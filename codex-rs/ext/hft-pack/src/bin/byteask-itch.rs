//! `byteask-itch`: Nasdaq TotalView-ITCH 5.0 capture files (2-byte length before every message).
//!
//! ```text
//! byteask-itch decode FILE      harness events, JSON Lines (the port_check recording format)
//! byteask-itch messages FILE    every message as JSON Lines, field names from the specification
//! byteask-itch synthetic SEED EVENTS OUT   write an invented, seeded session (for fixtures)
//! ```
//! A framing or decoding error stops with exit status 2 and names the message and byte offset.

use std::io::BufWriter;
use std::io::Write;
use std::process::ExitCode;

use byteask_hft_pack::itch50;
use byteask_hft_pack::read_events;
use byteask_hft_pack::read_messages;
use byteask_hft_pack::synthetic;

const USAGE: &str = "usage:\n  byteask-itch decode FILE\n  byteask-itch messages FILE\n  byteask-itch synthetic SEED EVENTS OUT";

fn open(path: &str) -> Result<std::io::BufReader<std::fs::File>, String> {
    std::fs::File::open(path)
        .map(std::io::BufReader::new)
        .map_err(|e| format!("cannot open {path}: {e}"))
}

fn emit<I: Iterator<Item = Result<serde_json::Value, String>>>(items: I) -> ExitCode {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for item in items {
        match item {
            Ok(v) => {
                if writeln!(out, "{v}").is_err() {
                    return ExitCode::from(2);
                }
            }
            Err(e) => {
                let _ = out.flush();
                eprintln!("byteask-itch: {e}");
                return ExitCode::from(2);
            }
        }
    }
    if out.flush().is_err() {
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["decode", file] => match open(file) {
            Ok(r) => emit(read_events(r)),
            Err(e) => {
                eprintln!("byteask-itch: {e}");
                ExitCode::from(2)
            }
        },
        ["messages", file] => match open(file) {
            Ok(r) => emit(read_messages(r).map(|m| m.map(|m| itch50::to_json(&m)))),
            Err(e) => {
                eprintln!("byteask-itch: {e}");
                ExitCode::from(2)
            }
        },
        ["synthetic", seed, events, out] => {
            let (Ok(seed), Ok(events)) = (seed.parse::<u64>(), events.parse::<usize>()) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            match std::fs::write(out, synthetic::to_file(&synthetic::session(seed, events))) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("byteask-itch: cannot write {out}: {e}");
                    ExitCode::from(2)
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
