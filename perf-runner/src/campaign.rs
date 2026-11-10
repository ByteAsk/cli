//! `compare` and `aa`: build (optional), check output (optional), take the
//! measurement lock, preflight, warm up, run randomised interleaved rounds
//! with a disturbance check on every run, TMA the measured region, analyse.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::bench;
use crate::gitbuild;
use crate::host;
use crate::host::LabConfig;
use crate::host::Window;
use crate::lock;
use crate::preflight;
use crate::report::CheckResult;
use crate::report::GuardResult;
use crate::report::HostInfo;
use crate::report::Report;
use crate::report::RoundRecord;
use crate::report::RunRecord;
use crate::report::SideInfo;
use crate::report::Verdict;
use crate::spawn;
use crate::spawn::Creds;
use crate::stats;
use crate::stats::Better;
use crate::stats::MetricVerdict;
use crate::tma;

#[derive(Debug, Clone)]
pub enum Source {
    Binaries { a: String, b: Option<String> },
    Git { repo: PathBuf, base: String, cand: Option<String>, build: String, bench: String },
    /// Trees the caller already built (perf-ut, optimize): `bench` runs with
    /// each tree as its working directory. The caller owns the trees.
    Prepared { a: SideInfo, a_dir: PathBuf, b: SideInfo, b_dir: PathBuf, bench: String },
}

#[derive(Debug, Clone)]
pub struct Options {
    pub aa: bool,
    pub source: Source,
    pub rounds: usize,
    pub floor_pct: f64,
    /// Per-metric minimum effect, overriding `floor_pct` (e.g. p99_ns=5).
    pub metric_floors: BTreeMap<String, f64>,
    pub confidence: f64,
    pub primary: Option<String>,
    pub tma: bool,
    pub tma_runs: usize,
    pub tma_args: String,
    /// Named regions (PERFLAB_REGION protocol); empty = the one default region.
    pub tma_regions: Vec<String>,
    /// `perf-runner tma`: TMA of side A only, no rounds.
    pub tma_only: bool,
    pub check: Option<String>,
    pub guards: Vec<String>,
    pub label_a: Option<String>,
    pub label_b: Option<String>,
    pub keep: bool,
    pub run_timeout: Duration,
    pub max_reruns: usize,
    pub lock_wait: Duration,
    /// After a disturbed round, wait up to this long for the host to go quiet.
    pub quiet_wait: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            aa: false,
            source: Source::Binaries { a: String::new(), b: None },
            rounds: 12,
            floor_pct: 2.0,
            metric_floors: BTreeMap::new(),
            confidence: 0.95,
            primary: None,
            tma: true,
            tma_runs: 5,
            tma_args: "--perf-ctl {ctl} {ack}".to_string(),
            tma_regions: Vec::new(),
            tma_only: false,
            check: None,
            guards: Vec::new(),
            label_a: None,
            label_b: None,
            keep: false,
            run_timeout: Duration::from_secs(600),
            max_reruns: 3,
            lock_wait: Duration::from_secs(1800),
            quiet_wait: Duration::from_secs(600),
        }
    }
}

/// Fewest rounds whose best possible sign-flip p-value is below 1 - confidence.
pub fn min_rounds(confidence: f64) -> usize {
    let alpha = 1.0 - confidence;
    (1..64).find(|&n| stats::sign_flip_min_p(n) < alpha).unwrap_or(64).max(6)
}

/// Per-run disturbance thresholds.
const RUN_SIBLING_BUSY: f64 = 0.02;
const RUN_ISOLATED_BUSY: f64 = 0.02;
const RUN_HOUSEKEEPING_BUSY: f64 = 0.60;
const RUN_FREQ_DRIFT: f64 = 0.01;
const RUN_IRQ_PER_S: f64 = 2.0;

/// Is a one-second idle window quiet enough to measure in?
pub fn quiet(w: &Window, cfg: &LabConfig) -> Option<String> {
    let hk = cfg.housekeeping.iter().map(|&c| w.busy_share(c)).sum::<f64>() / cfg.housekeeping.len().max(1) as f64;
    if hk > preflight::HOUSEKEEPING_BUSY_MAX {
        return Some(format!("housekeeping CPUs {:.0}% busy", hk * 100.0));
    }
    for &c in &cfg.isolated {
        if w.busy_ticks(c) > 1 {
            return Some(format!("isolated cpu {c} busy"));
        }
    }
    if w.steal.get(&cfg.measure_cpu).copied().unwrap_or(0) > 0 {
        return Some("steal time".to_string());
    }
    None
}

/// Block until the host is quiet for one second, or give up after `max`.
fn wait_quiet(cfg: &LabConfig, max: Duration) -> Result<f64, String> {
    let start = std::time::Instant::now();
    let mut last = String::new();
    loop {
        let a = host::Snapshot::take();
        std::thread::sleep(Duration::from_secs(1));
        let b = host::Snapshot::take();
        match quiet(&Window::between(&a, &b), cfg) {
            None => return Ok(start.elapsed().as_secs_f64()),
            Some(why) => {
                if why != last {
                    eprintln!("perf-runner: waiting for the host to go quiet ({why})");
                    last = why.clone();
                }
                if start.elapsed() >= max {
                    return Err(format!("the host did not go quiet within {}s ({why})", max.as_secs()));
                }
            }
        }
    }
}

/// Why a finished run cannot be trusted, if it cannot.
pub fn disturbance(w: &Window, cfg: &LabConfig, freq: Option<f64>, baseline: Option<f64>) -> Option<String> {
    let ticks = (w.seconds * w.ticks_per_s).max(1.0);
    let allow = |share: f64| (share * ticks).max(2.0);
    let steal = w.steal.get(&cfg.measure_cpu).copied().unwrap_or(0);
    if steal > 0 {
        return Some(format!("hypervisor steal time on cpu {} ({steal} tick(s))", cfg.measure_cpu));
    }
    if let Some(s) = cfg.sibling_cpu
        && w.busy_ticks(s) as f64 > allow(RUN_SIBLING_BUSY) {
            return Some(format!("SMT sibling cpu {s} busy ({} tick(s))", w.busy_ticks(s)));
        }
    for &c in &cfg.isolated {
        if c == cfg.measure_cpu || Some(c) == cfg.sibling_cpu {
            continue;
        }
        if w.busy_ticks(c) as f64 > allow(RUN_ISOLATED_BUSY) {
            return Some(format!("isolated cpu {c} busy ({} tick(s))", w.busy_ticks(c)));
        }
    }
    let core: Vec<usize> = std::iter::once(cfg.measure_cpu).chain(cfg.sibling_cpu).collect();
    let irqs: u64 = core.iter().map(|c| w.device_irqs.get(c).copied().unwrap_or(0)).sum();
    if irqs as f64 > (RUN_IRQ_PER_S * w.seconds).max(2.0) {
        return Some(format!("{irqs} device interrupt(s) on the measured core"));
    }
    let hk = cfg.housekeeping.iter().map(|&c| w.busy_share(c)).sum::<f64>() / cfg.housekeeping.len().max(1) as f64;
    if w.seconds > 0.2 && hk > RUN_HOUSEKEEPING_BUSY {
        return Some(format!("housekeeping CPUs {:.0}% busy (another job shares L3 and memory)", hk * 100.0));
    }
    if let (Some(f), Some(b)) = (freq, baseline)
        && (f / b - 1.0).abs() > RUN_FREQ_DRIFT {
            return Some(format!("core frequency {f:.3} GHz vs {b:.3} GHz at preflight"));
        }
    None
}

struct Side {
    info: SideInfo,
    argv: Vec<String>,
    cwd: PathBuf,
}

struct Ctx {
    cfg: LabConfig,
    creds: Option<Creds>,
    env: Vec<(String, String)>,
    scratch: PathBuf,
    /// The job's own writable directory (samples, perf output).
    job: PathBuf,
    baseline_ghz: Option<f64>,
    timeout: Duration,
}

impl Ctx {
    fn run(&self, side: &Side, extra: &[String], cpus: &[usize]) -> Result<(spawn::RunResult, Window, String), String> {
        let mut argv = side.argv.clone();
        argv.extend(extra.iter().cloned());
        let out = self.scratch.join("run.out");
        let err = self.scratch.join("run.err");
        let procs = self.cfg.procs_file();
        let job = spawn::Job {
            argv: &argv,
            cwd: &side.cwd,
            env: &self.env,
            cpus,
            cgroup_procs: Some(&procs),
            creds: self.creds.as_ref(),
            stdout: &out,
            stderr: &err,
            counters: true,
            timeout: self.timeout,
        };
        let before = host::Snapshot::take();
        let r = spawn::run(&job)?;
        let after = host::Snapshot::take();
        let stdout = std::fs::read_to_string(&out).unwrap_or_default();
        Ok((r, Window::between(&before, &after), stdout))
    }

    fn measured_run(&self, side: &Side) -> Result<RunRecord, String> {
        let (r, w, stdout) = self.run(side, &[], &[self.cfg.measure_cpu])?;
        let mut rec = RunRecord {
            exit: r.exit_code,
            signal: r.signal,
            timed_out: r.timed_out,
            wall_s: r.wall_s,
            user_s: r.user_s,
            sys_s: r.sys_s,
            freq_ghz: r.freq_ghz(),
            cycles: r.cycles,
            instructions: r.instructions,
            involuntary_switches: r.involuntary_switches,
            ..Default::default()
        };
        match bench::parse(&stdout) {
            Ok(Some(b)) => rec.metrics = b.metrics,
            Ok(None) => {}
            Err(e) => rec.perflab_error = Some(e),
        }
        // Raw samples, when the benchmark wrote them, give exact quantiles;
        // they replace the benchmark's own (often histogram-bucketed) ones.
        let samples = self.job.join("samples.bin");
        match crate::fsutil::read_job_file(&samples, self.creds.as_ref()).and_then(|b| b.map_or(Ok(None), |b| bench::samples_metrics(&b))) {
            Ok(Some(m)) => rec.metrics.extend(m),
            Ok(None) => {}
            Err(e) => rec.perflab_error = Some(e),
        }
        let _ = std::fs::remove_file(&samples);
        rec.disturbed = disturbance(&w, &self.cfg, rec.freq_ghz, self.baseline_ghz);
        Ok(rec)
    }
}

fn run_failed(r: &RunRecord) -> Option<String> {
    if r.timed_out {
        Some("timed out".to_string())
    } else if let Some(s) = r.signal {
        Some(format!("killed by signal {s}"))
    } else if r.exit != Some(0) {
        Some(format!("exit status {}", r.exit.unwrap_or(-1)))
    } else {
        r.perflab_error.clone()
    }
}

/// Who jobs run as and the environment they get (for callers that build).
pub fn job_context() -> (Option<Creds>, Vec<(String, String)>) {
    let creds = Creds::from_sudo();
    let env = job_env(creds.as_ref());
    (creds, env)
}

/// The environment every job gets: the caller's, with the caller's PATH and
/// identity. Identical for A and B (env size moves stack alignment).
fn job_env(creds: Option<&Creds>) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    if let Ok(p) = std::env::var("PERFLAB_USER_PATH") {
        env.insert("PATH".to_string(), p);
    }
    env.remove("PERFLAB_USER_PATH");
    for k in ["SUDO_COMMAND", "SUDO_USER", "SUDO_UID", "SUDO_GID", "SUDO_HOME", "MAIL"] {
        env.remove(k);
    }
    if let Some(c) = creds {
        env.insert("HOME".to_string(), c.home.clone());
        env.insert("USER".to_string(), c.name.clone());
        env.insert("LOGNAME".to_string(), c.name.clone());
    }
    env.into_iter().collect()
}

pub fn empty_report(opts: &Options) -> Report {
    Report {
        schema: crate::report::SCHEMA,
        mode: if opts.tma_only { "tma" } else if opts.aa { "aa" } else { "compare" },
        verdict: Verdict::NotRun,
        verdict_reason: String::new(),
        summary: Vec::new(),
        primary_metric: None,
        confidence: opts.confidence,
        floor_pct: opts.floor_pct,
        rounds: 0,
        host: HostInfo {
            hostname: host::hostname(),
            cpu_model: host::cpu_model(),
            kernel: host::kernel(),
            ..Default::default()
        },
        preflight: None,
        a: SideInfo::default(),
        b: SideInfo::default(),
        metrics: Vec::new(),
        regressions: Vec::new(),
        guards: Vec::new(),
        check: CheckResult::default(),
        tma: tma::TmaReport::not_run("not reached".to_string()),
        tma_regions: Vec::new(),
        rounds_detail: Vec::new(),
        order_seed: 0,
        measure_lock: None,
        notes: Vec::new(),
        started_utc: host::now_utc(),
        finished_utc: String::new(),
        runner_version: env!("CARGO_PKG_VERSION"),
    }
}

fn finish(mut r: Report, verdict: Verdict, reason: impl Into<String>) -> Report {
    r.verdict = verdict;
    r.verdict_reason = reason.into();
    r.finished_utc = host::now_utc();
    r.build_summary();
    r
}

fn quote_words(argv: &[String]) -> String {
    argv.iter()
        .map(|a| if a.chars().all(|c| c.is_ascii_alphanumeric() || "/._-=+:,".contains(c)) { a.clone() } else { format!("'{}'", a.replace('\'', r"'\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run(opts: &Options, runner_exe: &Path) -> Report {
    let mut rep = empty_report(opts);
    let need = min_rounds(opts.confidence);
    if opts.rounds < need {
        return finish(rep, Verdict::NotRun, format!("--rounds {} is too few: at {:.0}% confidence the test needs at least {need}", opts.rounds, opts.confidence * 100.0));
    }
    let cfg = match LabConfig::load(&host::config_path()) {
        Ok(c) => c,
        Err(e) => {
            rep.preflight = Some(preflight::run(runner_exe));
            return finish(rep, Verdict::NotRun, format!("host refused: lab not set up ({e}); run `sudo perf-runner setup`"));
        }
    };
    rep.host.measured_cpu = Some(cfg.measure_cpu);
    rep.host.sibling_cpu = cfg.sibling_cpu;
    rep.host.housekeeping_cpus = Some(host::format_cpulist(&cfg.housekeeping));

    let creds = Creds::from_sudo();
    if creds.is_none() {
        rep.notes.push("not started through sudo: jobs run as root (prefer running perf-runner as a normal user)".to_string());
    }
    let env = job_env(creds.as_ref());
    let stamp = format!("{}-{}", host::now_utc().replace([':', '-'], ""), std::process::id());
    // Builds under /var/tmp/perflab; job output on tmpfs (no disk I/O, so no
    // nvme interrupts, from the measured core). See fsutil for the ownership rules.
    let ws = match crate::fsutil::workspace(&stamp, creds.as_ref()) {
        Ok(w) => w,
        Err(e) => return finish(rep, Verdict::NotRun, format!("cannot set up the run's directories: {e}")),
    };
    let (run_dir, scratch) = (ws.run_dir.clone(), ws.scratch.clone());

    // ---- sides -------------------------------------------------------------
    let invoking_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut cleanup: Vec<(PathBuf, PathBuf)> = Vec::new();
    let sides: Result<(Side, Side), String> = (|| {
        match &opts.source {
            Source::Prepared { a, a_dir, b, b_dir, bench } => {
                let argv = spawn::split_command(bench)?;
                let mk = |info: &SideInfo, dir: &PathBuf| Side {
                    info: SideInfo { command: argv.clone(), cwd: dir.to_string_lossy().into_owned(), ..info.clone() },
                    argv: argv.clone(),
                    cwd: dir.clone(),
                };
                Ok((mk(a, a_dir), mk(b, b_dir)))
            }
            Source::Binaries { a, b } => {
                let a_argv = spawn::split_command(a)?;
                let b_argv = match (opts.aa, b) {
                    (true, _) | (false, None) => a_argv.clone(),
                    (false, Some(b)) => spawn::split_command(b)?,
                };
                if !opts.aa && b.is_none() {
                    return Err("compare needs --b (or use `aa` to compare a binary with itself)".to_string());
                }
                let mk = |label: String, argv: Vec<String>| Side {
                    info: SideInfo { label, command: argv.clone(), cwd: invoking_cwd.to_string_lossy().into_owned(), ..Default::default() },
                    argv,
                    cwd: invoking_cwd.clone(),
                };
                Ok((mk(opts.label_a.clone().unwrap_or("A".into()), a_argv), mk(opts.label_b.clone().unwrap_or(if opts.aa { "A (again)".into() } else { "B".into() }), b_argv)))
            }
            Source::Git { repo, base, cand, build, bench } => {
                let argv = spawn::split_command(bench)?;
                let mut built = Vec::new();
                let refs: Vec<(String, String)> = if opts.aa {
                    vec![("a".into(), base.clone())]
                } else {
                    let c = cand.clone().ok_or("compare in git mode needs --cand")?;
                    vec![("a".into(), base.clone()), ("b".into(), c)]
                };
                for (slot, reference) in &refs {
                    let dest = ws.trees.join(slot);
                    let commit = gitbuild::add_worktree(repo, reference, &dest, creds.as_ref(), &env)?;
                    cleanup.push((repo.clone(), dest.clone()));
                    eprintln!("perf-runner: building {reference} ({}) in {}", &commit[..commit.len().min(12)], dest.display());
                    let info = gitbuild::build(&dest, build, creds.as_ref(), &env, &run_dir.join(format!("build-{slot}.log")));
                    built.push(Side {
                        info: SideInfo {
                            label: reference.clone(),
                            reference: Some(reference.clone()),
                            commit: Some(commit),
                            command: argv.clone(),
                            cwd: dest.to_string_lossy().into_owned(),
                            build: Some(info),
                        },
                        argv: argv.clone(),
                        cwd: dest,
                    });
                }
                let a = built.remove(0);
                let mut b = if built.is_empty() {
                    Side { info: a.info.clone(), argv: a.argv.clone(), cwd: a.cwd.clone() }
                } else {
                    built.remove(0)
                };
                if opts.aa {
                    b.info.label = format!("{} (again)", b.info.label);
                }
                let mut a = a;
                if let Some(l) = &opts.label_a {
                    a.info.label = l.clone();
                }
                if let Some(l) = &opts.label_b {
                    b.info.label = l.clone();
                }
                Ok((a, b))
            }
        }
    })();
    let do_cleanup = |cleanup: &[(PathBuf, PathBuf)]| {
        if !opts.keep {
            for (repo, dest) in cleanup {
                gitbuild::remove_worktree(repo, dest, creds.as_ref(), &env);
            }
            let _ = std::fs::remove_dir_all(&run_dir);
        }
        let _ = std::fs::remove_dir_all(&scratch);
    };
    let (a, b) = match sides {
        Ok(s) => s,
        Err(e) => {
            do_cleanup(&cleanup);
            return finish(rep, Verdict::NotRun, e);
        }
    };
    rep.a = a.info.clone();
    rep.b = b.info.clone();
    for (s, name) in [(&a, "A"), (&b, "B")] {
        if let Some(bi) = &s.info.build
            && !bi.ok {
                if opts.keep {
                    rep.notes.push(format!("build log: {}", bi.log.clone().unwrap_or_default()));
                }
                do_cleanup(&cleanup);
                return finish(rep, Verdict::NotRun, format!("the build of {name} ({}) failed; last lines:\n{}", s.info.label, bi.log_tail));
            }
    }

    // ---- output check (functional, no lock) ----------------------------------
    if let Some(check) = &opts.check {
        rep.check.ran = true;
        rep.check.command = Some(check.clone());
        let expand = |s: &Side| check.replace("{bin}", &quote_words(&s.argv[..1])).replace("{cmd}", &quote_words(&s.argv));
        let ra = gitbuild::capture(&a.cwd, &expand(&a), creds.as_ref(), &env);
        let rb = gitbuild::capture(&b.cwd, &expand(&b), creds.as_ref(), &env);
        match (ra, rb) {
            (Ok((true, oa)), Ok((true, ob))) => {
                if oa == ob {
                    rep.check.ok = Some(true);
                } else {
                    let (la, lb): (Vec<&str>, Vec<&str>) = (oa.lines().collect(), ob.lines().collect());
                    let i = (0..la.len().max(lb.len())).find(|&i| la.get(i) != lb.get(i)).unwrap_or(0);
                    rep.check.ok = Some(false);
                    rep.check.first_diff_line = Some(i + 1);
                    rep.check.a = la.get(i).map(|s| s.to_string());
                    rep.check.b = lb.get(i).map(|s| s.to_string());
                    do_cleanup(&cleanup);
                    return finish(rep, Verdict::Rejected, format!("output changed at line {}: A {:?} vs B {:?}", i + 1, la.get(i).unwrap_or(&"<end>"), lb.get(i).unwrap_or(&"<end>")));
                }
            }
            (ra, rb) => {
                let detail = format!("A: {:?}; B: {:?}", ra.map(|x| x.0), rb.map(|x| x.0));
                rep.check.detail = Some(detail.clone());
                do_cleanup(&cleanup);
                return finish(rep, Verdict::Invalid, format!("the output check did not run cleanly ({detail})"));
            }
        }
    }

    // ---- measurement (under the lock) ------------------------------------------
    let lock = match lock::acquire(opts.lock_wait) {
        Ok(l) => l,
        Err(e) => {
            do_cleanup(&cleanup);
            return finish(rep, Verdict::NotRun, e);
        }
    };
    rep.measure_lock = Some(lock.how.clone());
    // Load from other jobs is transient: when the preflight fails ONLY on
    // load-type checks, wait for quiet and ask again, up to --quiet-wait. A
    // static misconfiguration fails at once.
    let started = std::time::Instant::now();
    let mut waited = 0.0;
    let pf = loop {
        if let Ok(w) = wait_quiet(&cfg, opts.quiet_wait.saturating_sub(started.elapsed())) {
            waited += w;
        }
        eprintln!("perf-runner: preflight");
        let pf = preflight::run(runner_exe);
        let transient = !pf.ok && pf.failures().iter().all(|c| preflight::TRANSIENT.contains(&c.name));
        if !transient || started.elapsed() >= opts.quiet_wait {
            break pf;
        }
        eprintln!("perf-runner: preflight failed on load only ({}); waiting and trying again", pf.failure_summary());
    };
    if waited > 1.5 {
        rep.notes.push(format!("waited {waited:.0}s for other jobs to finish before the preflight passed"));
    }
    rep.host.freq_ghz = pf.freq_ghz;
    let ok = pf.ok;
    let why = pf.failure_summary();
    rep.preflight = Some(pf);
    if !ok {
        drop(lock);
        do_cleanup(&cleanup);
        return finish(rep, Verdict::NotRun, format!("host refused: {why}"));
    }
    let mut env = env.clone();
    env.push(("PERFLAB_SAMPLES".to_string(), ws.job.join("samples.bin").to_string_lossy().into_owned()));
    let ctx = Ctx {
        baseline_ghz: rep.host.freq_ghz,
        cfg: cfg.clone(),
        creds: creds.clone(),
        env: env.clone(),
        scratch: scratch.clone(),
        job: ws.job.clone(),
        timeout: opts.run_timeout,
    };

    if opts.tma_only {
        eprintln!("perf-runner: TMA of one build");
        // One untimed run first: page cache and lazy binding, as for rounds.
        if let Err(e) = ctx.measured_run(&a) {
            drop(lock);
            do_cleanup(&cleanup);
            return finish(rep, Verdict::NotRun, format!("warmup run failed: {e}"));
        }
        let mut regions = run_tma_regions(&ctx, &a, &b, opts, true);
        drop(lock);
        do_cleanup(&cleanup);
        rep.tma = regions.remove(0);
        rep.tma_regions = regions;
        let all: Vec<&tma::TmaReport> = std::iter::once(&rep.tma).chain(rep.tma_regions.iter()).collect();
        if let Some(bad) = all.iter().find(|t| t.status != "OK") {
            let why = bad.reason.clone().unwrap_or_default();
            return finish(rep, Verdict::NotRun, format!("TMA not run: {why}"));
        }
        let heads = all.iter().map(|t| format!("{}: {}", t.region, t.a_headline.as_deref().unwrap_or("-"))).collect::<Vec<_>>().join("; ");
        return finish(rep, Verdict::NoClaim, format!("TMA of one build, nothing compared. {heads}"));
    }
    let fail_run = |rep: Report, which: &str, label: &str, why: String| {
        finish(rep, Verdict::Invalid, format!("{which} ({label}) failed: {why}; timings of a failing run mean nothing"))
    };

    // Warmup: one untimed run per side (page cache, lazy binding).
    for (s, name) in [(&a, "A"), (&b, "B")] {
        eprintln!("perf-runner: warmup {name}");
        match ctx.measured_run(s) {
            Ok(r) => {
                if let Some(e) = run_failed(&r) {
                    drop(lock);
                    do_cleanup(&cleanup);
                    return fail_run(rep, name, &s.info.label, e);
                }
            }
            Err(e) => {
                drop(lock);
                do_cleanup(&cleanup);
                return fail_run(rep, name, &s.info.label, e);
            }
        }
    }

    // Balanced random order: half the rounds AB, half BA.
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) ^ u64::from(std::process::id());
    rep.order_seed = seed;
    let mut rng = stats::Rng::new(seed);
    let mut order: Vec<bool> = (0..opts.rounds).map(|i| i % 2 == 0).collect();
    rng.shuffle(&mut order);
    let mut total_reruns = 0;
    for (i, &a_first) in order.iter().enumerate() {
        let mut discarded = Vec::new();
        loop {
            eprintln!("perf-runner: round {}/{} ({})", i + 1, opts.rounds, if a_first { "AB" } else { "BA" });
            let pair: Result<(RunRecord, RunRecord), (String, &str)> = (|| {
                let first = if a_first { (&a, "A") } else { (&b, "B") };
                let second = if a_first { (&b, "B") } else { (&a, "A") };
                let r1 = ctx.measured_run(first.0).map_err(|e| (e, first.1))?;
                if let Some(e) = run_failed(&r1) {
                    return Err((e, first.1));
                }
                let r2 = ctx.measured_run(second.0).map_err(|e| (e, second.1))?;
                if let Some(e) = run_failed(&r2) {
                    return Err((e, second.1));
                }
                Ok(if a_first { (r1, r2) } else { (r2, r1) })
            })();
            let (ra, rb) = match pair {
                Ok(p) => p,
                Err((e, which)) => {
                    drop(lock);
                    do_cleanup(&cleanup);
                    let label = if which == "A" { a.info.label.clone() } else { b.info.label.clone() };
                    return fail_run(rep, which, &label, e);
                }
            };
            let disturbed = ra.disturbed.clone().map(|d| format!("A: {d}")).or_else(|| rb.disturbed.clone().map(|d| format!("B: {d}")));
            match disturbed {
                Some(d) if discarded.len() < opts.max_reruns && total_reruns < opts.rounds => {
                    eprintln!("perf-runner: round {} disturbed ({d}); running it again once the host is quiet", i + 1);
                    discarded.push(d);
                    total_reruns += 1;
                    match wait_quiet(&cfg, opts.quiet_wait) {
                        Ok(w) if w > 1.5 => rep.notes.push(format!("waited {w:.0}s for the host to go quiet before re-running round {}", i + 1)),
                        Ok(_) => {}
                        Err(e) => {
                            drop(lock);
                            do_cleanup(&cleanup);
                            return finish(rep, Verdict::NotRun, format!("host disturbed during measurement: {e}"));
                        }
                    }
                }
                Some(d) => {
                    drop(lock);
                    do_cleanup(&cleanup);
                    rep.notes.push(format!("round {}: {}", i + 1, discarded.join("; ")));
                    return finish(rep, Verdict::NotRun, format!("host disturbed during measurement: round {} stayed disturbed after {} re-run(s) ({d}); {total_reruns} re-run(s) in total", i + 1, discarded.len()));
                }
                None => {
                    rep.rounds_detail.push(RoundRecord {
                        round: i + 1,
                        order: if a_first { "AB" } else { "BA" },
                        rerun: discarded.len(),
                        discarded,
                        a: ra,
                        b: rb,
                    });
                    break;
                }
            }
        }
    }
    rep.rounds = rep.rounds_detail.len();
    if total_reruns > 0 {
        rep.notes.push(format!("{total_reruns} disturbed round(s) were discarded and run again (whole rounds, so A and B stay paired)"));
    }

    // ---- TMA ---------------------------------------------------------------
    if opts.tma {
        eprintln!("perf-runner: TMA");
        let mut regions = run_tma_regions(&ctx, &a, &b, opts, false);
        rep.tma = regions.remove(0);
        rep.tma_regions = regions;
    } else {
        rep.tma = tma::TmaReport::not_run("disabled with --no-tma".to_string());
    }
    drop(lock);

    // ---- analysis ------------------------------------------------------------
    let rounds = &rep.rounds_detail;
    let mut names: Vec<String> = rounds
        .first()
        .map(|r| r.a.metrics.keys().filter(|k| rounds.iter().all(|r| r.a.metrics.contains_key(*k) && r.b.metrics.contains_key(*k))).cloned().collect())
        .unwrap_or_default();
    if rounds.iter().any(|r| r.a.metrics.is_empty()) {
        rep.notes.push("the benchmark printed no PERFLAB line: only whole-run wall time is compared; tail latency NOT RUN".to_string());
    }
    if let Some(n) = rounds.first().and_then(|r| r.a.metrics.get("samples")) {
        rep.notes.push(format!("latency quantiles computed by the runner from the raw samples the benchmark wrote ({n:.0} per run), not from its own histogram"));
    }
    names.push("wall_s".to_string());
    names.sort_by_key(|n| bench::display_rank(n));
    let units: BTreeMap<String, String> = BTreeMap::new();
    let get = |r: &RunRecord, n: &str| if n == "wall_s" { r.wall_s } else { r.metrics.get(n).copied().unwrap_or(f64::NAN) };
    for n in &names {
        if opts.guards.contains(n) {
            continue;
        }
        let av: Vec<f64> = rounds.iter().map(|r| get(&r.a, n)).collect();
        let bv: Vec<f64> = rounds.iter().map(|r| get(&r.b, n)).collect();
        let better = bench::better_of(n);
        let floor = opts.metric_floors.get(n).copied().unwrap_or(opts.floor_pct);
        rep.metrics.push(stats::analyse(n, better, bench::unit_of(n, &units), &av, &bv, opts.confidence, floor));
    }
    // Guards: B must not exceed A.
    let mut guard_violation = None;
    let mut guard_missing = None;
    for g in &opts.guards {
        let have = rounds.iter().all(|r| r.a.metrics.contains_key(g) && r.b.metrics.contains_key(g));
        if !have {
            rep.guards.push(GuardResult { metric: g.clone(), a_median: None, b_median: None, ok: None });
            guard_missing.get_or_insert(g.clone());
            continue;
        }
        let am = stats::median(&rounds.iter().map(|r| r.a.metrics[g]).collect::<Vec<_>>());
        let bm = stats::median(&rounds.iter().map(|r| r.b.metrics[g]).collect::<Vec<_>>());
        let ok = bm <= am;
        if !ok {
            guard_violation.get_or_insert(format!("{g} went from {am} to {bm} (must not increase)"));
        }
        rep.guards.push(GuardResult { metric: g.clone(), a_median: Some(am), b_median: Some(bm), ok: Some(ok) });
    }

    let primary = opts.primary.clone().unwrap_or_else(|| {
        if names.iter().any(|n| n == "p50_ns") {
            "p50_ns".to_string()
        } else {
            names.iter().find(|n| *n != "wall_s" && bench::better_of(n) != Better::Info && !opts.guards.contains(n)).cloned().unwrap_or_else(|| "wall_s".to_string())
        }
    });
    rep.primary_metric = Some(primary.clone());
    let prim = rep.metrics.iter().find(|m| m.name == primary).cloned();
    // A regression elsewhere makes a win MIXED only when the whole family of
    // secondary metrics is held at alpha (Bonferroni): one-at-a-time tests at
    // 95% let a noisy tail metric (a p99 with 6% noise per round) "regress" in
    // ~6% of A/A runs; family-wise the rate is about 3%.
    rep.regressions = regressions(&rep.metrics, &primary, opts.confidence);
    if let Some(m) = prim.as_ref()
        && m.noise_pct.is_finite() && opts.aa {
            rep.notes.push(format!(
                "A/A noise on {}: {:.2}% per round; this many rounds detect a {:.1}% change; {} rounds would resolve the {}% floor",
                primary,
                m.noise_pct,
                m.min_detectable_pct,
                m.rounds_needed_for_floor.map_or("?".to_string(), |n| n.max(need).to_string()),
                opts.floor_pct
            ));
        }
    do_cleanup(&cleanup);

    if let Some(v) = guard_violation {
        return finish(rep, Verdict::Rejected, format!("hot-path guard violated: {v}"));
    }
    if let Some(g) = guard_missing {
        return finish(rep, Verdict::NotRun, format!("guard metric {g} is not in the benchmark's PERFLAB line, so it could not be checked"));
    }
    let Some(m) = prim else {
        return finish(rep, Verdict::NotRun, format!("primary metric {primary} was not reported in every round"));
    };
    if m.better == Better::Info {
        return finish(rep, Verdict::NoClaim, format!("primary metric {primary} has no direction (name it *_ns, *_s or throughput)"));
    }
    let mut v = Verdict::from_metric(m.verdict);
    let mut reason = format!("{}: {}", crate::report::short_name(&m.name), m.reason);
    // A hot path that got faster at the primary metric but measurably worse
    // elsewhere (the tail, say) is not a win: MIXED. Whole-run wall time does
    // not count (it includes loading and setup).
    let tail_losses: Vec<String> = rep.regressions.iter().filter(|x| x.as_str() != "wall_s").cloned().collect();
    v = combine(v, &tail_losses);
    if v == Verdict::Mixed {
        reason = format!(
            "{reason}; but {} got measurably worse",
            tail_losses
                .iter()
                .map(|x| {
                    let m = rep.metrics.iter().find(|m| &m.name == x);
                    format!("{} ({:+.1}%)", crate::report::short_name(x), m.map_or(f64::NAN, |m| m.change_pct))
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if opts.aa && v != Verdict::NoClaim {
        rep.notes.push("an A/A run made a claim: treat this host's results as suspect until A/A says NO CLAIM again".to_string());
    }
    finish(rep, v, reason)
}

/// Secondary metrics that measurably got worse, the family held at alpha.
pub fn regressions(metrics: &[stats::MetricResult], primary: &str, confidence: f64) -> Vec<String> {
    let secondary: Vec<&stats::MetricResult> = metrics.iter().filter(|m| m.name != primary && m.name != "wall_s" && m.better != Better::Info).collect();
    let alpha_k = (1.0 - confidence) / secondary.len().max(1) as f64;
    secondary.iter().filter(|m| m.verdict == MetricVerdict::Slower && m.p_value < alpha_k).map(|m| m.name.clone()).collect()
}

/// The overall verdict from the primary metric's and the other metrics'
/// regressions: a win anywhere else paired with a measurable loss is MIXED.
pub fn combine(primary: Verdict, other_losses: &[String]) -> Verdict {
    if primary == Verdict::Faster && !other_losses.is_empty() { Verdict::Mixed } else { primary }
}

/// TMA of the measured region for A and B (or A alone with `single`). With
/// `region`, the benchmark gets `PERFLAB_REGION=<name>` and must enable the
/// counters only around that region.
fn run_tma(ctx: &Ctx, a: &Side, b: &Side, opts: &Options, region: Option<&str>, single: bool) -> tma::TmaReport {
    let ctl = ctx.scratch.join("perf-ctl.fifo");
    let ack = ctx.scratch.join("perf-ack.fifo");
    for p in [&ctl, &ack] {
        let _ = std::fs::remove_file(p);
        let c = std::ffi::CString::new(p.to_string_lossy().as_bytes()).unwrap_or_default();
        // SAFETY: mkfifo on a NUL-terminated path.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return tma::TmaReport::not_run(format!("mkfifo {}: {}", p.display(), std::io::Error::last_os_error()));
        }
        if let Some(cr) = &ctx.creds {
            let _ = std::os::unix::fs::lchown(p, Some(cr.uid), Some(cr.gid));
        }
    }
    let extra: Vec<String> = match spawn::split_command(&opts.tma_args.replace("{ctl}", &ctl.to_string_lossy()).replace("{ack}", &ack.to_string_lossy())) {
        Ok(v) => v,
        Err(_) if opts.tma_args.trim().is_empty() => Vec::new(),
        Err(e) => return tma::TmaReport::not_run(format!("bad --tma-args: {e}")),
    };
    let out = ctx.job.join("tma.json");
    let mut env = ctx.env.clone();
    env.push(("PERFLAB_CTL_FIFO".into(), ctl.to_string_lossy().into_owned()));
    env.push(("PERFLAB_ACK_FIFO".into(), ack.to_string_lossy().into_owned()));
    if let Some(r) = region {
        env.push(("PERFLAB_REGION".into(), r.to_string()));
    }
    let tma_ctx = Ctx { cfg: ctx.cfg.clone(), creds: ctx.creds.clone(), env, scratch: ctx.scratch.clone(), job: ctx.job.clone(), baseline_ghz: ctx.baseline_ghz, timeout: ctx.timeout };
    let one = |s: &Side| -> Result<tma::TmaRun, String> {
        let _ = std::fs::remove_file(&out); // perf (the job's user) recreates it
        let mut argv = vec![
            "perf".to_string(),
            "stat".into(),
            "-j".into(),
            "-M".into(),
            "TopdownL1,TopdownL2".into(),
            "--control".into(),
            format!("fifo:{},{}", ctl.display(), ack.display()),
            "--delay=-1".into(),
            "-o".into(),
            out.to_string_lossy().into_owned(),
            "--".into(),
            "taskset".into(),
            "-c".into(),
            ctx.cfg.measure_cpu.to_string(),
        ];
        argv.extend(s.argv.iter().cloned());
        argv.extend(extra.iter().cloned());
        let side = Side { info: s.info.clone(), argv, cwd: s.cwd.clone() };
        let (r, _w, _stdout) = tma_ctx.run(&side, &[], &[ctx.cfg.control_cpu])?;
        if !r.ok() {
            let err = std::fs::read_to_string(ctx.scratch.join("run.err")).unwrap_or_default();
            return Err(format!("perf stat run {}: {}", r.describe_failure(), err.lines().rev().take(3).collect::<Vec<_>>().join(" | ")));
        }
        let text = crate::fsutil::read_job_file(&out, ctx.creds.as_ref())?.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        tma::parse_perf_json(&text)
    };
    let mut ar = Vec::new();
    let mut br = Vec::new();
    for i in 0..opts.tma_runs.max(1) {
        let pair: Vec<(&Side, bool)> = if single {
            vec![(a, true)]
        } else if i % 2 == 0 {
            vec![(a, true), (b, false)]
        } else {
            vec![(b, false), (a, true)]
        };
        for (s, is_a) in pair {
            match one(s) {
                Ok(r) => {
                    if is_a { ar.push(r) } else { br.push(r) }
                }
                Err(e) => return tma::TmaReport::not_run(format!("{} side: {e}", if is_a { "A" } else { "B" })),
            }
        }
    }
    let mut rep = if single { tma::summarise(&ar, &ar) } else { tma::summarise(&ar, &br) };
    if single {
        rep.b.clear();
        rep.b_headline = None;
        rep.b_slots = None;
        rep.delta_pp.clear();
        rep.delta_slots_pct.clear();
    }
    rep.region = region.map_or_else(|| "perf-ctl".to_string(), |r| r.to_string());
    rep
}

/// TMA for every requested region ([] = the one default region).
fn run_tma_regions(ctx: &Ctx, a: &Side, b: &Side, opts: &Options, single: bool) -> Vec<tma::TmaReport> {
    if opts.tma_regions.is_empty() {
        return vec![run_tma(ctx, a, b, opts, None, single)];
    }
    opts.tma_regions.iter().map(|r| run_tma(ctx, a, b, opts, Some(r), single)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LabConfig {
        LabConfig {
            version: 1,
            cgroup: "/x".into(),
            housekeeping: vec![0, 4],
            isolated: vec![1, 2, 3, 5, 6, 7],
            measure_cpu: 2,
            sibling_cpu: Some(6),
            control_cpu: 1,
            freq_ghz: Some(3.0),
            created_utc: String::new(),
        }
    }

    fn window(secs: f64) -> Window {
        let mut w = Window { seconds: secs, ticks_per_s: 100.0, ..Default::default() };
        for c in 0..8 {
            w.busy.insert(c, 0);
            w.steal.insert(c, 0);
            w.device_irqs.insert(c, 0);
        }
        w.busy.insert(2, (secs * 100.0) as u64);
        w
    }

    #[test]
    fn min_rounds_by_confidence() {
        assert_eq!(min_rounds(0.95), 6);
        assert_eq!(min_rounds(0.99), 8);
    }

    #[test]
    fn disturbance_rules() {
        let c = cfg();
        assert_eq!(disturbance(&window(1.0), &c, Some(3.0), Some(3.0)), None);
        let mut w = window(1.0);
        w.steal.insert(2, 1);
        assert!(disturbance(&w, &c, None, None).unwrap().contains("steal"));
        let mut w = window(1.0);
        w.busy.insert(6, 5);
        assert!(disturbance(&w, &c, None, None).unwrap().contains("sibling"));
        // Two ticks of a kworker on the sibling in a short run is tolerated.
        let mut w = window(0.3);
        w.busy.insert(6, 2);
        assert_eq!(disturbance(&w, &c, None, None), None);
        let mut w = window(1.0);
        w.busy.insert(3, 10);
        assert!(disturbance(&w, &c, None, None).unwrap().contains("isolated cpu 3"));
        let mut w = window(1.0);
        w.device_irqs.insert(2, 9);
        assert!(disturbance(&w, &c, None, None).unwrap().contains("interrupt"));
        let mut w = window(1.0);
        w.busy.insert(0, 90);
        w.busy.insert(4, 90);
        assert!(disturbance(&w, &c, None, None).unwrap().contains("housekeeping"));
        assert!(disturbance(&window(1.0), &c, Some(2.9), Some(3.0)).unwrap().contains("frequency"));
    }

    #[test]
    fn regressions_are_family_wise() {
        let mk = |name: &str, p: f64, v: MetricVerdict| {
            let mut m = stats::analyse(name, Better::Lower, None, &[100.0; 8], &[100.0; 8], 0.95, 2.0);
            m.p_value = p;
            m.verdict = v;
            m
        };
        let ms = vec![
            mk("p50_ns", 0.001, MetricVerdict::Faster),
            mk("p99_ns", 0.03, MetricVerdict::Slower), // 0.03 > 0.05 / 2: not family-wise significant
            mk("p999_ns", 0.01, MetricVerdict::Slower), // 0.01 < 0.025
            mk("wall_s", 0.0001, MetricVerdict::Slower), // never counts
        ];
        assert_eq!(regressions(&ms, "p50_ns", 0.95), vec!["p999_ns".to_string()]);
    }

    #[test]
    fn mixed_only_when_a_win_carries_a_loss() {
        let loss = vec!["p99_ns".to_string()];
        assert_eq!(combine(Verdict::Faster, &loss), Verdict::Mixed);
        assert_eq!(combine(Verdict::Faster, &[]), Verdict::Faster);
        assert_eq!(combine(Verdict::Slower, &loss), Verdict::Slower);
        assert_eq!(combine(Verdict::NoClaim, &loss), Verdict::NoClaim);
    }

    #[test]
    fn quiet_rules() {
        let c = cfg();
        let mut w = window(1.0);
        w.busy.insert(2, 0);
        assert_eq!(quiet(&w, &c), None);
        w.busy.insert(0, 80);
        assert!(quiet(&w, &c).unwrap().contains("housekeeping"));
        let mut w = window(1.0);
        w.busy.insert(2, 0);
        w.busy.insert(6, 3);
        assert!(quiet(&w, &c).unwrap().contains("isolated cpu 6"));
    }

    #[test]
    fn job_env_is_side_independent_and_drops_sudo() {
        let e = job_env(None);
        assert!(!e.iter().any(|(k, _)| k == "SUDO_COMMAND" || k == "PERFLAB_USER_PATH"));
    }
}
