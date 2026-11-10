//! Two git refs -> two worktrees -> two builds, all as the calling user.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;

use serde::Serialize;

use crate::spawn::Creds;

#[derive(Debug, Clone, Serialize, Default)]
pub struct BuildInfo {
    pub ok: bool,
    pub seconds: f64,
    pub log: Option<String>,
    pub log_tail: String,
}

/// A `Command` that runs as `creds` (when given) with exactly `env`.
pub fn user_command(program: &str, creds: Option<&Creds>, env: &[(String, String)]) -> Command {
    let mut c = Command::new(program);
    c.env_clear();
    c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    if let Some(cr) = creds {
        let (uid, gid, groups) = (cr.uid, cr.gid, cr.groups.clone());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            c.pre_exec(move || {
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    c
}

fn git(repo: &Path, args: &[&str], creds: Option<&Creds>, env: &[(String, String)]) -> Result<String, String> {
    let out = user_command("git", creds, env)
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve `reference` to a commit and check it out (detached) at `dest`.
pub fn add_worktree(repo: &Path, reference: &str, dest: &Path, creds: Option<&Creds>, env: &[(String, String)]) -> Result<String, String> {
    let commit = git(repo, &["rev-parse", "--verify", &format!("{reference}^{{commit}}")], creds, env)?;
    git(repo, &["worktree", "add", "--detach", "--force", &dest.to_string_lossy(), &commit], creds, env)?;
    Ok(commit)
}

pub fn remove_worktree(repo: &Path, dest: &Path, creds: Option<&Creds>, env: &[(String, String)]) {
    let _ = git(repo, &["worktree", "remove", "--force", &dest.to_string_lossy()], creds, env);
    let _ = git(repo, &["worktree", "prune"], creds, env);
}

/// Run `cmd` with `bash -c` in `dir`; output to `log`.
pub fn build(dir: &Path, cmd: &str, creds: Option<&Creds>, env: &[(String, String)], log: &Path) -> BuildInfo {
    let start = Instant::now();
    let file = match crate::fsutil::create_root_file(log, creds) {
        Ok(f) => f,
        Err(e) => {
            return BuildInfo { ok: false, log_tail: e, ..Default::default() };
        }
    };
    let err = file.try_clone();
    let status = match err {
        Ok(err) => user_command("bash", creds, env)
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(err)
            .status(),
        Err(e) => Err(e),
    };
    let tail = tail_of(log, 30);
    match status {
        Ok(s) => BuildInfo {
            ok: s.success(),
            seconds: start.elapsed().as_secs_f64(),
            log: Some(log.to_string_lossy().into_owned()),
            log_tail: tail,
        },
        Err(e) => BuildInfo {
            ok: false,
            seconds: start.elapsed().as_secs_f64(),
            log: Some(log.to_string_lossy().into_owned()),
            log_tail: format!("could not start bash: {e}"),
        },
    }
}

/// Run a shell command in `dir`, capture stdout (for the output check).
pub fn capture(dir: &Path, cmd: &str, creds: Option<&Creds>, env: &[(String, String)]) -> Result<(bool, String), String> {
    let out = user_command("bash", creds, env)
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("bash: {e}"))?;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

pub fn tail_of(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = text.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}
