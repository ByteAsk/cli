//! `byteask-port-check`: the port_check tool from the command line.
//!
//! ```text
//! byteask-port-check check REQUEST.json   (or - for stdin)   prints the result JSON
//! byteask-port-check adapters DIR                             writes port_harness.py/.hpp
//! byteask-port-check describe                                 prints the tool spec JSON
//! ```
//! Exit status of `check`: 0 PASS, 1 FAIL, 2 NOT RUN, 3 NO CLAIM.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use byteask_port_check::tool;

const USAGE: &str = "usage:\n  byteask-port-check check REQUEST.json|-\n  byteask-port-check adapters DIR\n  byteask-port-check describe";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, file] if cmd == "check" => {
            let text = if file == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).map(|_| s)
            } else {
                std::fs::read_to_string(file)
            };
            let result = match text {
                Ok(t) => tool::run_request_json(&t),
                Err(e) => tool::not_run_result(format!("cannot read the request {file}: {e}")),
            };
            match serde_json::to_string_pretty(&result) {
                Ok(s) => println!("{s}"),
                Err(e) => println!("{{\"tool\":\"port_check\",\"status\":\"NOT RUN\",\"reason\":\"{e}\"}}"),
            }
            ExitCode::from(tool::exit_code(result.status))
        }
        [cmd, dir] if cmd == "adapters" => match byteask_port_check::write_adapters(&PathBuf::from(dir)) {
            Ok(paths) => {
                for p in paths {
                    println!("{}", p.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("cannot write the adapters into {dir}: {e}");
                ExitCode::from(2)
            }
        },
        [cmd] if cmd == "describe" => {
            println!("{}", serde_json::to_string_pretty(&tool::tool_spec()).unwrap_or_default());
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
