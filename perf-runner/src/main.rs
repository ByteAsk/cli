//! perf-runner: the ByteAsk perf lab runner.
//!
//! See `perf-runner help`. Runs on the measurement host.

mod bench;
mod fsutil;
mod campaign;
mod gitbuild;
mod host;
mod lock;
mod optimize;
mod preflight;
mod report;
mod setup;
mod spawn;
mod stats;
mod tma;
mod ut;

use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "perf-runner — measure on an isolated, checked core; never claim inside the noise.

USAGE
  perf-runner preflight [--json]
  perf-runner compare --a '<cmd>' --b '<cmd>' [options]
  perf-runner compare --repo DIR --base REF --cand REF --build '<shell cmd>' --bench '<cmd>' [options]
  perf-runner aa --a '<cmd>' [options]
  perf-runner aa --repo DIR --ref REF --build '<shell cmd>' --bench '<cmd>' [options]
  perf-runner tma --a '<cmd>' [--regions a,b] [--runs N]     TMA L1+L2 of one build's measured region(s)
      (or --repo DIR --ref REF --build ... --bench ...)
  perf-runner ut --file perf-ut.toml --repo DIR --base REF --head REF [--json F|-] [--md CARD.md]
      preregistered perf unit tests for a merge; writes the pull-request perf card
      exit 0 PASS, 2 NOT RUN, 4 FAIL
  perf-runner optimize --repo DIR --base REF --candidate REF [--candidate REF ...] --build CMD --bench CMD
      [--explore-rounds 8] [--confirm-rounds 16] [--confidence 0.99] [--calibrate-aa N [--aa-candidates K]]
      explore all candidates, freeze the best, confirm it on fresh measurements
  sudo perf-runner setup [--measure-cpu N]      put the host in measurement shape (runtime, no reboot)
  sudo perf-runner teardown                     undo setup

OPTIONS (compare / aa)
  --rounds N          measured rounds, order randomised per round [12]
  --floor PCT         minimum effect for FASTER/SLOWER [2]
  --metric-floor M=P  per-metric minimum effect, e.g. p99_ns=5 (repeatable)
  --confidence C      CI and test level [0.95]; use 0.99 to gate merges
  --primary METRIC    metric that sets the verdict [p50_ns, else the first one]
  --no-tma            skip TMA
  --tma-runs N        TMA runs per side [5]; a category counts as changed only if the two sides'
                      ranges do not overlap and its share moved >= 1 pp
  --tma-args 'ARGS'   appended for TMA runs ['--perf-ctl {ctl} {ack}']
  --tma-regions a,b   TMA per named region: the benchmark gets PERFLAB_REGION=<name> and enables
                      the counters only around that region
  --check '<sh cmd>'  B's stdout must equal A's ({bin}, {cmd} expand per side)
  --guard METRIC      B's median must not exceed A's (repeatable)
  --json FILE|-       write the JSON report ('-' = stdout)
  --md FILE           write the Markdown report
  --label-a / --label-b NAME
  --timeout SEC       per run [600]
  --quiet-wait SEC    after a disturbed round, wait this long for the host to go quiet [600]
  --keep              keep worktrees and scratch files

The benchmark reports its own numbers with one stdout line:
  PERFLAB {\"p50_ns\": 412.0, \"p99_ns\": 1830.0, \"p999_ns\": 4100.0, \"throughput\": 2.35e6, \"throughput_unit\": \"msg/s\"}
Verdicts: FASTER, SLOWER, NO_CLAIM, MIXED (better at the primary metric, measurably worse at
another), REJECTED (output check or guard failed), INVALID (a run failed), NOT_RUN (host refused,
build failed).
Exit: 0 verdict, 2 NOT_RUN, 3 INVALID, 1 usage error.";

fn usage(msg: &str) -> ! {
    eprintln!("perf-runner: {msg}\n\n{HELP}");
    std::process::exit(1);
}

struct Args {
    cmd: String,
    kv: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String]) -> Args {
        let cmd = raw.first().cloned().unwrap_or_else(|| "help".to_string());
        let flags = ["--json-flag", "--no-tma", "--tma", "--keep", "--help"];
        let mut kv = Vec::new();
        let mut i = 1;
        while i < raw.len() {
            let a = &raw[i];
            if !a.starts_with("--") {
                usage(&format!("unexpected argument {a:?}"));
            }
            if let Some((k, v)) = a.split_once('=') {
                kv.push((k.to_string(), Some(v.to_string())));
            } else if flags.contains(&a.as_str()) || (a == "--json" && raw.get(i + 1).is_none_or(|n| n.starts_with("--"))) {
                kv.push((a.clone(), None));
            } else {
                let Some(v) = raw.get(i + 1) else { usage(&format!("{a} needs a value")) };
                kv.push((a.clone(), Some(v.clone())));
                i += 1;
            }
            i += 1;
        }
        Args { cmd, kv }
    }

    fn get(&self, k: &str) -> Option<String> {
        self.kv.iter().rev().find(|(x, _)| x == k).and_then(|(_, v)| v.clone())
    }

    fn all(&self, k: &str) -> Vec<String> {
        self.kv.iter().filter(|(x, _)| x == k).filter_map(|(_, v)| v.clone()).collect()
    }

    fn has(&self, k: &str) -> bool {
        self.kv.iter().any(|(x, _)| x == k)
    }

    fn num<T: std::str::FromStr>(&self, k: &str) -> Option<T> {
        self.get(k).map(|v| v.parse().unwrap_or_else(|_| usage(&format!("{k} {v:?} is not a number"))))
    }

    fn check_known(&self, known: &[&str]) {
        for (k, _) in &self.kv {
            if !known.contains(&k.as_str()) {
                usage(&format!("unknown option {k}"));
            }
        }
    }
}

/// Not root: run again through `sudo -n`, keeping the caller's PATH for the
/// jobs. Returns only if that is not possible.
fn reexec_as_root(raw: &[String]) {
    if unsafe { libc::geteuid() } == 0 || std::env::var_os("PERFLAB_REEXEC").is_some() {
        return;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("perf-runner"));
    let path = std::env::var("PATH").unwrap_or_default();
    let status = std::process::Command::new("sudo")
        .arg("-n")
        .arg("-E")
        .arg(format!("PERFLAB_USER_PATH={path}"))
        .arg("PERFLAB_REEXEC=1")
        .arg(&exe)
        .args(raw)
        .status();
    match status {
        Ok(s) if s.code() == Some(1) && !sudo_works() => {
            eprintln!("perf-runner: passwordless sudo is not available; continuing without root (preflight will refuse)");
        }
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => eprintln!("perf-runner: could not run sudo ({e}); continuing without root (preflight will refuse)"),
    }
}

fn sudo_works() -> bool {
    std::process::Command::new("sudo").args(["-n", "true"]).status().is_ok_and(|s| s.success())
}

/// Reports go where the caller asked, written with the caller's rights (root
/// never writes a path the caller chose).
fn write_report(path: &str, data: &[u8]) {
    if let Err(e) = fsutil::write_as_caller(path, data, spawn::Creds::from_sudo().as_ref()) {
        eprintln!("perf-runner: {e}");
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("__spin") {
        // Calibration workload (preflight): must stay this cheap and this first.
        let n: u64 = raw.get(1).and_then(|v| v.parse().ok()).unwrap_or(preflight::CALIBRATION_ITERS);
        println!("{}", preflight::spin(n));
        return;
    }
    let args = Args::parse(&raw);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    match args.cmd.as_str() {
        "help" | "--help" | "-h" => println!("{HELP}"),
        "version" | "--version" => println!("perf-runner {}", env!("CARGO_PKG_VERSION")),
        "preflight" => {
            args.check_known(&["--json"]);
            reexec_as_root(&raw);
            let lock = lock::acquire(Duration::from_secs(600));
            let pf = preflight::run(&exe);
            drop(lock);
            if args.has("--json") {
                println!("{}", serde_json::to_string_pretty(&pf).unwrap_or_default());
            } else {
                for c in &pf.checks {
                    let fix = c.fix.as_deref().filter(|_| c.status != preflight::Status::Pass).map(|f| format!("\n      fix: {f}")).unwrap_or_default();
                    println!("{:<5} {:<22} {}{fix}", c.status.word(), c.name, c.detail);
                }
                println!("\n{}", if pf.ok { "host is fit to measure" } else { "REFUSED: this host is not fit to measure (see FAIL lines)" });
            }
            std::process::exit(if pf.ok { 0 } else { 2 });
        }
        "setup" => {
            args.check_known(&["--measure-cpu"]);
            reexec_as_root(&raw);
            let opts = setup::SetupOptions { measure_cpu: args.num("--measure-cpu"), cgroup_name: "perflab".to_string() };
            match setup::setup(&opts, &exe) {
                Ok(cfg) => {
                    println!("{}", serde_json::to_string_pretty(&cfg).unwrap_or_default());
                    println!("set up: measure on cpu {} (sibling {:?} kept idle), OS on {}", cfg.measure_cpu, cfg.sibling_cpu, host::format_cpulist(&cfg.housekeeping));
                }
                Err(e) => {
                    eprintln!("perf-runner setup: {e}");
                    std::process::exit(2);
                }
            }
        }
        "teardown" => {
            args.check_known(&[]);
            reexec_as_root(&raw);
            match setup::teardown("perflab") {
                Ok(m) => println!("{m}"),
                Err(e) => {
                    eprintln!("perf-runner teardown: {e}");
                    std::process::exit(2);
                }
            }
        }
        "compare" | "aa" | "tma" => {
            let tma_only = args.cmd == "tma";
            let aa = args.cmd == "aa" || tma_only;
            args.check_known(&[
                "--a", "--b", "--repo", "--base", "--cand", "--ref", "--build", "--bench", "--rounds", "--floor", "--confidence", "--primary", "--tma", "--no-tma",
                "--tma-runs", "--tma-args", "--check", "--guard", "--json", "--md", "--label-a", "--label-b", "--timeout", "--keep", "--quiet-wait", "--metric-floor", "--tma-regions", "--regions", "--runs",
            ]);
            let source = if let Some(repo) = args.get("--repo") {
                let base = if aa { args.get("--ref").or(args.get("--base")) } else { args.get("--base") };
                campaign::Source::Git {
                    repo: PathBuf::from(repo),
                    base: base.unwrap_or_else(|| usage(if aa { "aa in git mode needs --ref" } else { "compare in git mode needs --base and --cand" })),
                    cand: args.get("--cand"),
                    build: args.get("--build").unwrap_or_else(|| usage("git mode needs --build")),
                    bench: args.get("--bench").unwrap_or_else(|| usage("git mode needs --bench")),
                }
            } else {
                campaign::Source::Binaries {
                    a: args.get("--a").unwrap_or_else(|| usage("give --a '<cmd>' (and --b for compare), or --repo/--base/--cand")),
                    b: args.get("--b"),
                }
            };
            if !aa && matches!(&source, campaign::Source::Binaries { b: None, .. }) {
                usage("compare needs --b; to compare a binary with itself use `perf-runner aa`");
            }
            let d = campaign::Options::default();
            let confidence: f64 = args.num("--confidence").unwrap_or(d.confidence);
            if !(0.5..1.0).contains(&confidence) {
                usage("--confidence must be in [0.5, 1)");
            }
            let mut metric_floors = std::collections::BTreeMap::new();
            for mf in args.all("--metric-floor") {
                let (k, v) = mf.split_once('=').unwrap_or_else(|| usage("--metric-floor wants NAME=PCT, e.g. p99_ns=5"));
                let v: f64 = v.parse().unwrap_or_else(|_| usage("--metric-floor wants NAME=PCT, e.g. p99_ns=5"));
                metric_floors.insert(k.to_string(), v);
            }
            let regions: Vec<String> = args
                .get("--tma-regions")
                .or(args.get("--regions"))
                .map(|r| r.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            let opts = campaign::Options {
                aa,
                tma_only,
                tma_regions: regions,
                metric_floors,
                source,
                rounds: args.num("--rounds").unwrap_or(d.rounds),
                floor_pct: args.num("--floor").unwrap_or(d.floor_pct),
                confidence,
                primary: args.get("--primary"),
                tma: !args.has("--no-tma"),
                tma_runs: args.num("--tma-runs").or(args.num("--runs")).unwrap_or(d.tma_runs),
                tma_args: args.get("--tma-args").unwrap_or(d.tma_args.clone()),
                check: args.get("--check"),
                guards: args.all("--guard"),
                label_a: args.get("--label-a"),
                label_b: args.get("--label-b"),
                keep: args.has("--keep"),
                run_timeout: Duration::from_secs(args.num("--timeout").unwrap_or(600)),
                quiet_wait: Duration::from_secs(args.num("--quiet-wait").unwrap_or(600)),
                ..d
            };
            reexec_as_root(&raw);
            let report = campaign::run(&opts, &exe);
            let json = serde_json::to_string_pretty(&report).unwrap_or_default();
            let to_stdout = args.get("--json").as_deref() == Some("-") || (args.has("--json") && args.get("--json").is_none());
            if let Some(path) = args.get("--json").filter(|p| p != "-") {
                write_report(&path, json.as_bytes());
            }
            if let Some(path) = args.get("--md") {
                write_report(&path, report.markdown().as_bytes());
            }
            if to_stdout {
                println!("{json}");
                for l in &report.summary {
                    eprintln!("{l}");
                }
            } else {
                for l in &report.summary {
                    println!("{l}");
                }
            }
            std::process::exit(report.verdict.exit_code());
        }
        "ut" => {
            args.check_known(&["--file", "--repo", "--base", "--head", "--json", "--md", "--keep"]);
            let ua = ut::UtArgs {
                file: PathBuf::from(args.get("--file").unwrap_or_else(|| usage("ut needs --file"))),
                repo: PathBuf::from(args.get("--repo").unwrap_or_else(|| usage("ut needs --repo"))),
                base: args.get("--base").unwrap_or_else(|| usage("ut needs --base")),
                head: args.get("--head").unwrap_or_else(|| usage("ut needs --head")),
                keep: args.has("--keep"),
            };
            reexec_as_root(&raw);
            match ut::run(&ua, &exe) {
                Ok(u) => {
                    let json = serde_json::to_string_pretty(&u).unwrap_or_default();
                    let card = ut::card(&u);
                    if let Some(p) = args.get("--json").filter(|p| p != "-") {
                        write_report(&p, json.as_bytes());
                    }
                    if let Some(p) = args.get("--md") {
                        write_report(&p, card.as_bytes());
                    }
                    if args.get("--json").as_deref() == Some("-") {
                        println!("{json}");
                        eprint!("{card}");
                    } else {
                        print!("{card}");
                    }
                    std::process::exit(match u.gate {
                        ut::Gate::Pass => 0,
                        ut::Gate::NotRun => 2,
                        ut::Gate::Fail => 4,
                    });
                }
                Err(e) => {
                    eprintln!("perf-runner ut: {e}");
                    std::process::exit(2);
                }
            }
        }
        "optimize" => {
            args.check_known(&[
                "--repo", "--base", "--candidate", "--build", "--bench", "--explore-rounds", "--confirm-rounds", "--confidence", "--floor", "--primary",
                "--calibrate-aa", "--aa-candidates", "--json", "--keep",
            ]);
            let oa = optimize::OptArgs {
                repo: PathBuf::from(args.get("--repo").unwrap_or_else(|| usage("optimize needs --repo"))),
                base: args.get("--base").unwrap_or_else(|| usage("optimize needs --base")),
                candidates: args.all("--candidate"),
                build: args.get("--build").unwrap_or_else(|| usage("optimize needs --build")),
                bench: args.get("--bench").unwrap_or_else(|| usage("optimize needs --bench")),
                explore_rounds: args.num("--explore-rounds").unwrap_or(8),
                confirm_rounds: args.num("--confirm-rounds").unwrap_or(16),
                confidence: args.num("--confidence").unwrap_or(0.99),
                floor_pct: args.num("--floor").unwrap_or(2.0),
                primary: args.get("--primary"),
                calibrate_aa: args.num("--calibrate-aa"),
                aa_candidates: args.num("--aa-candidates").unwrap_or(3),
                keep: args.has("--keep"),
            };
            reexec_as_root(&raw);
            match optimize::run(&oa, &exe) {
                Ok(r) => {
                    let json = serde_json::to_string_pretty(&r).unwrap_or_default();
                    if let Some(p) = args.get("--json").filter(|p| p != "-") {
                        write_report(&p, json.as_bytes());
                    }
                    if args.get("--json").as_deref() == Some("-") {
                        println!("{json}");
                    }
                    for l in optimize::summary(&r) {
                        eprintln!("{l}");
                    }
                }
                Err(e) => {
                    eprintln!("perf-runner optimize: {e}");
                    std::process::exit(2);
                }
            }
        }
        other => usage(&format!("unknown command {other:?}")),
    }
}
