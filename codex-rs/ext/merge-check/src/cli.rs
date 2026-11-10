//! `byteask checks run | sign | gate | candidate | keygen | profile` (the CI side; the
//! person-only `skip` lives with the local results store in the toolbox).
//!
//! Secrets never travel on the command line: the enterprise service token and the platform
//! tokens come from files or named environment variables, and the runner key from a 0600 file.

use std::io::IsTerminal as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::bail;

use crate::binding::Candidate;
use crate::gate;
use crate::perf::PerfEvidence;
use crate::platform;
use crate::run;
use crate::sign;
use crate::sign::RunnerKind;
use crate::verify;

// Parsed once per process; boxing the larger argument sets buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, clap::Subcommand)]
pub enum MergeCommand {
    /// Run the required checks for head merged into base (CI job without signing keys).
    Run(RunArgs),
    /// Sign a run report or a bench perf report with this runner's enrolled key.
    Sign(SignArgs),
    /// Decide the merge from signed results only, and post it to the code platform.
    Gate(GateArgs),
    /// Print the merge candidate (merged tree, content key and a commit for it) as JSON.
    Candidate(CandidateArgs),
    /// Make a runner key file and print its public key for the admin console.
    Keygen(KeygenArgs),
    /// Propose or check the repository's build profile (.byteask/checks.toml).
    #[command(subcommand)]
    Profile(ProfileCommand),
}

#[derive(Debug, clap::Args)]
pub struct CandidateRefs {
    /// The repository (default: the current directory).
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// The protected base branch, e.g. origin/main.
    #[arg(long)]
    pub base: String,
    /// The change being merged (default: HEAD).
    #[arg(long, default_value = "HEAD")]
    pub head: String,
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub refs: CandidateRefs,
    /// Where to write the run report (JSON).
    #[arg(long)]
    pub out: PathBuf,
    /// Scratch directory for the checkout and reports (default: the system temp dir).
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct SignArgs {
    #[command(flatten)]
    pub refs: CandidateRefs,
    /// This runner's Ed25519 key file (PKCS#8 PEM, mode 600).
    #[arg(long)]
    pub key: PathBuf,
    /// The run report from `byteask checks run` (CI runners).
    #[arg(long, conflicts_with = "perf_ut")]
    pub report: Option<PathBuf>,
    /// A `perf-runner ut --json` report (bench runners).
    #[arg(long)]
    pub perf_ut: Option<PathBuf>,
    /// Where to write the signed result.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Also file it in this CI-side cache directory, under its content key.
    #[arg(long)]
    pub cache: Option<PathBuf>,
    /// The company's enterprise server, to record the signature now.
    #[arg(long)]
    pub enterprise: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct GateArgs {
    #[command(flatten)]
    pub refs: CandidateRefs,
    /// A signed result, or a directory of them (repeatable).
    #[arg(long)]
    pub evidence: Vec<PathBuf>,
    /// The CI-side cache directory (signed results under <cache>/<content key>/).
    #[arg(long)]
    pub cache: Option<PathBuf>,
    /// The company's enterprise server, which knows the enrolled runner keys.
    #[arg(long)]
    pub enterprise: String,
    /// File holding the enterprise service token (else $BYTEASK_ENTERPRISE_TOKEN).
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Approvals as JSON (for platforms whose own approvals carry no version).
    #[arg(long)]
    pub approvals: Option<PathBuf>,
    /// Write the decision as JSON here.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// GitHub `owner/name`: post a check run (token in $GITHUB_TOKEN or --github-token-env).
    #[arg(long)]
    pub github_repo: Option<String>,
    #[arg(long, default_value = "https://api.github.com")]
    pub github_api: String,
    /// The pull request, to read its reviews.
    #[arg(long)]
    pub github_pull: Option<u64>,
    /// File holding the GitHub token (else the variable named by --github-token-env).
    #[arg(long)]
    pub github_token_file: Option<PathBuf>,
    #[arg(long, default_value = "GITHUB_TOKEN")]
    pub github_token_env: String,
    /// GitLab project id: post a commit status and a merge-request note.
    #[arg(long)]
    pub gitlab_project: Option<String>,
    #[arg(long, default_value = "https://gitlab.com/api/v4")]
    pub gitlab_api: String,
    #[arg(long)]
    pub gitlab_mr: Option<u64>,
    /// File holding the GitLab token (else the variable named by --gitlab-token-env).
    #[arg(long)]
    pub gitlab_token_file: Option<PathBuf>,
    #[arg(long, default_value = "GITLAB_TOKEN")]
    pub gitlab_token_env: String,
    /// Jenkins: write byteask-merge-check.md and byteask-merge-check.xml (JUnit) here.
    #[arg(long)]
    pub jenkins_out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct CandidateArgs {
    #[command(flatten)]
    pub refs: CandidateRefs,
}

#[derive(Debug, clap::Args)]
pub struct KeygenArgs {
    /// The key file to create (never overwritten).
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(Debug, clap::Subcommand)]
pub enum ProfileCommand {
    /// Propose a profile from CMake presets, a compile database or CMakeLists.txt.
    Propose {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Check the profile on a branch (default: the working tree's file).
    Check {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Read it from this commit or branch instead of the working tree.
        #[arg(long)]
        rev: Option<String>,
    },
}

fn read_secret(file: Option<&Path>, env: &str) -> anyhow::Result<String> {
    if let Some(path) = file {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let meta = std::fs::metadata(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            if meta.mode() & 0o077 != 0 {
                bail!(
                    "{} is readable by other users; chmod 600 it first",
                    path.display()
                );
            }
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        return Ok(text.trim().to_string());
    }
    std::env::var(env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("no token: pass a token file or set {env}"))
}

/// Run one command; the process exit status.
#[allow(clippy::print_stdout, clippy::print_stderr)]
pub async fn run_command(command: MergeCommand) -> anyhow::Result<i32> {
    match command {
        MergeCommand::Run(args) => {
            let options = run::RunOptions {
                repo: args.refs.repo,
                base: args.refs.base,
                head: args.refs.head,
                work_dir: args.work_dir,
            };
            let report = run::run_checks(&options)
                .await
                .map_err(anyhow::Error::msg)?;
            let text = serde_json::to_string_pretty(&report)?;
            std::fs::write(&args.out, format!("{text}\n"))
                .with_context(|| format!("could not write {}", args.out.display()))?;
            for result in &report.results {
                eprintln!("{}: {}", result.check, result.render_line());
            }
            eprintln!(
                "Run report for merge candidate {} written to {}. It counts at merge only once a CI runner key signs it (`byteask checks sign`).",
                &report.binding.candidate_key()[..12],
                args.out.display()
            );
            Ok(0)
        }
        MergeCommand::Sign(args) => sign_command(args).await,
        MergeCommand::Gate(args) => gate_command(args).await,
        MergeCommand::Candidate(args) => {
            let candidate = Candidate::compute(&args.refs.repo, &args.refs.base, &args.refs.head)
                .map_err(anyhow::Error::msg)?;
            let commit = if candidate.binding.merge_tree.is_empty() {
                None
            } else {
                Some(
                    crate::git::commit_tree(
                        &args.refs.repo,
                        &candidate.binding.merge_tree,
                        &[
                            &candidate.binding.base_commit,
                            &candidate.binding.head_commit,
                        ],
                    )
                    .map_err(anyhow::Error::msg)?,
                )
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "binding": candidate.binding,
                    "candidate_key": candidate.binding.candidate_key(),
                    "content_key": candidate.binding.content_key(),
                    "candidate_commit": commit,
                    "conflict": candidate.conflict,
                    "changed_files": candidate.changed.len(),
                }))?
            );
            Ok(0)
        }
        MergeCommand::Keygen(args) => {
            let key = sign::write_new_key(&args.out).map_err(anyhow::Error::msg)?;
            println!("Runner key written to {} (mode 600).", args.out.display());
            println!("Key id:     {}", key.key_id());
            println!("Public key: {}", key.public_b64());
            println!(
                "Enrol the public key in your ByteAsk admin console (runner keys, kind ci or bench) before this runner signs anything."
            );
            Ok(0)
        }
        MergeCommand::Profile(ProfileCommand::Propose { repo }) => {
            let proposal = checks_profile::propose(&repo);
            eprintln!("Proposed from {}.", proposal.source);
            print!("{}", proposal.text);
            Ok(0)
        }
        MergeCommand::Profile(ProfileCommand::Check { repo, rev }) => {
            let text = match &rev {
                Some(rev) => crate::git::show(&repo, rev, checks_profile::PATH)
                    .with_context(|| format!("{rev} has no {}", checks_profile::PATH))?,
                None => std::fs::read_to_string(repo.join(checks_profile::PATH))
                    .with_context(|| format!("could not read {}", checks_profile::PATH))?,
            };
            match checks_profile::Profile::parse(&text) {
                Ok(_) => {
                    println!("{} is valid.", checks_profile::PATH);
                    Ok(0)
                }
                Err(err) => {
                    println!("{err}");
                    Ok(1)
                }
            }
        }
    }
}

#[allow(clippy::print_stderr)]
async fn sign_command(args: SignArgs) -> anyhow::Result<i32> {
    let key = sign::RunnerKey::load(&args.key).map_err(anyhow::Error::msg)?;
    let candidate = Candidate::compute(&args.refs.repo, &args.refs.base, &args.refs.head)
        .map_err(anyhow::Error::msg)?;
    let candidate_key = candidate.binding.candidate_key();
    let (kind, payload) = match (&args.report, &args.perf_ut) {
        (Some(path), None) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let report: run::RunReport =
                serde_json::from_str(&text).context("not a `byteask checks run` report")?;
            // Only a report for exactly this candidate is signed. The build
            // environment is the run job's own (it can differ from this job's runner image).
            if !report.binding.same_candidate(&candidate.binding)
                || report.content_key != report.binding.content_key()
            {
                bail!(
                    "the report is for another merge candidate (candidate {}, this one is {}); not signed",
                    &report.binding.candidate_key()[..12],
                    &candidate_key[..12]
                );
            }
            if report
                .results
                .iter()
                .any(|result| result.content_key.as_deref() != Some(report.content_key.as_str()))
            {
                bail!("a result inside the report is for another version; not signed");
            }
            (RunnerKind::Ci, serde_json::to_string(&report)?)
        }
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let ut: serde_json::Value = serde_json::from_str(&text).context("not JSON")?;
            let repo = args.refs.repo.clone();
            let evidence = PerfEvidence::bind(ut, &candidate.binding, |commit| {
                crate::git::tree_of(&repo, commit).ok()
            })
            .map_err(anyhow::Error::msg)?;
            (RunnerKind::Bench, serde_json::to_string(&evidence)?)
        }
        _ => bail!("pass --report (a CI run report) or --perf-ut (a bench report)"),
    };
    let envelope = key.sign(kind, payload, run::now_ms());
    if let Some(base) = &args.enterprise {
        verify::attest(base, &envelope)
            .await
            .map_err(anyhow::Error::msg)?;
        eprintln!("Recorded with the enterprise server.");
    } else {
        eprintln!(
            "Not recorded with an enterprise server (--enterprise): if this runner is retired before the merge check sees this result, it will not count."
        );
    }
    let text = serde_json::to_string_pretty(&envelope)?;
    if let Some(out) = &args.out {
        std::fs::write(out, format!("{text}\n"))
            .with_context(|| format!("could not write {}", out.display()))?;
    }
    if let Some(cache) = &args.cache {
        let path = gate::cache_path(cache, &candidate_key, &envelope);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, format!("{text}\n"))
            .with_context(|| format!("could not write {}", path.display()))?;
        eprintln!("Filed in the cache as {}.", path.display());
    }
    if args.out.is_none() && args.cache.is_none() {
        println!("{text}");
    }
    Ok(0)
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn gate_command(args: GateArgs) -> anyhow::Result<i32> {
    let candidate = Candidate::compute(&args.refs.repo, &args.refs.base, &args.refs.head)
        .map_err(anyhow::Error::msg)?;
    let key = candidate.binding.candidate_key();
    let token = read_secret(args.token_file.as_deref(), "BYTEASK_ENTERPRISE_TOKEN")?;
    let verifier =
        verify::EnterpriseVerifier::new(&args.enterprise, &token).map_err(anyhow::Error::msg)?;
    let mut paths = args.evidence.clone();
    if let Some(cache) = &args.cache {
        paths.push(cache.join(&key));
    }
    let envelopes = gate::collect_envelopes(&paths, &key);
    let evidence = gate::sort_evidence(envelopes, &verifier, &candidate.binding).await;
    let mut approvals = Vec::new();
    if let Some(path) = &args.approvals {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        approvals
            .extend(serde_json::from_str::<Vec<gate::Approval>>(&text).context("approvals file")?);
    }
    let github = match &args.github_repo {
        Some(repo) => Some(platform::GitHub {
            api: args.github_api.clone(),
            repo: repo.clone(),
            token: read_secret(args.github_token_file.as_deref(), &args.github_token_env)?,
            pull: args.github_pull,
        }),
        None => None,
    };
    if let Some(github) = &github {
        approvals.extend(github.approvals().await.map_err(anyhow::Error::msg)?);
    }
    let decision = gate::decide(&candidate, &evidence, &approvals);
    let markdown = gate::report_markdown(&decision);
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&decision)?)?;
    }
    let mut post_errors = Vec::new();
    if let Some(github) = &github
        && let Err(err) = github.post(&decision, &markdown).await
    {
        post_errors.push(err);
    }
    if let Some(project) = &args.gitlab_project {
        let gitlab = platform::GitLab {
            api: args.gitlab_api.clone(),
            project: project.clone(),
            token: read_secret(args.gitlab_token_file.as_deref(), &args.gitlab_token_env)?,
            merge_request: args.gitlab_mr,
        };
        if let Err(err) = gitlab.post(&decision, &markdown).await {
            post_errors.push(err);
        }
    }
    if let Some(dir) = &args.jenkins_out {
        std::fs::create_dir_all(dir)?;
        let (summary, junit) = platform::jenkins_files(&decision, &markdown);
        std::fs::write(dir.join("byteask-merge-check.md"), summary)?;
        std::fs::write(dir.join("byteask-merge-check.xml"), junit)?;
    }
    println!("{}", decision.summary);
    if std::io::stdout().is_terminal() {
        for why in &decision.ignored {
            println!("  did not count: {why}");
        }
    }
    for err in &post_errors {
        eprintln!("could not post the status: {err}");
    }
    if !post_errors.is_empty() {
        // The status page does not show the decision: never report success.
        return Ok(2);
    }
    Ok(if decision.allowed { 0 } else { 1 })
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
