//! Compiler caching for sub-agent worktree builds (the reproduction-test writer and the engine
//! checks of its tests). Uses the ccache binary shipped in the ByteAsk package (`<package>/bin/ccache`,
//! e.g. `/opt/byteask/bin/ccache`) or one on `PATH`; without one, builds simply run uncached.
//!
//! Two hooks, so both CMake and plain-make/autotools builds hit the cache:
//! * a masquerade directory (`cc`, `gcc`, `c++`, `g++`, `clang`, ... -> ccache) first on `PATH`;
//! * `CMAKE_<LANG>_COMPILER_LAUNCHER` (read from the environment by CMake >= 3.17) pointing at a
//!   small launcher that skips ccache when the compiler already is the masquerade link, so a
//!   compiler is never wrapped twice.
//!
//! The cache is shared by every worktree of one session (`CCACHE_DIR`), and
//! `CCACHE_BASEDIR` = the session's scratch dir + `CCACHE_NOHASHDIR` make object files
//! reusable across worktrees at different paths.

use std::path::Path;
use std::path::PathBuf;

const COMPILER_NAMES: &[&str] = &["cc", "c++", "gcc", "g++", "clang", "clang++"];

/// Locate a usable ccache binary.
pub fn find_ccache() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("BYTEASK_CCACHE") {
        let p = PathBuf::from(explicit);
        return is_executable(&p).then_some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("bin").join("ccache"));
        candidates.push(dir.join("ccache"));
    }
    candidates.push(PathBuf::from("/opt/byteask/bin/ccache"));
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("ccache"));
        }
    }
    candidates.into_iter().find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Compiler names on `PATH` to masquerade: the plain names plus versioned ones (`gcc-9`,
/// `clang++-14`, `x86_64-linux-gnu-g++`).
fn compilers_on_path() -> Vec<String> {
    let mut names: Vec<String> = COMPILER_NAMES.iter().map(|s| (*s).to_string()).collect();
    let Some(path) = std::env::var_os("PATH") else {
        return names;
    };
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let base = name.rsplit_once("-linux-gnu-").map_or(name.as_str(), |(_, b)| b);
            let family = base.split('-').next().unwrap_or(base);
            if COMPILER_NAMES.contains(&family)
                && (base == family || base[family.len() + 1..].chars().all(|c| c.is_ascii_digit() || c == '.'))
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names
}

/// Environment for builds that should use the shared cache. Creates the masquerade dir and
/// launcher under `scratch`. Returns an empty list when no ccache is available.
pub fn build_env(scratch: &Path, parallel_jobs: usize) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("CMAKE_BUILD_PARALLEL_LEVEL".to_string(), parallel_jobs.to_string()),
        ("MAKEFLAGS".to_string(), format!("-j{parallel_jobs}")),
    ];
    // The ccache masquerade (symlinks + an sh launcher) is Unix-only; elsewhere builds run uncached.
    #[cfg(unix)]
    add_ccache_env(scratch, &mut env);
    #[cfg(not(unix))]
    let _ = scratch;
    env
}

#[cfg(unix)]
fn add_ccache_env(scratch: &Path, env: &mut Vec<(String, String)>) {
    let Some(ccache) = find_ccache() else {
        return;
    };
    let masq = scratch.join("ccache-bin");
    if std::fs::create_dir_all(&masq).is_err() {
        return;
    }
    for name in compilers_on_path() {
        let link = masq.join(&name);
        if !link.exists() {
            let _ = std::os::unix::fs::symlink(&ccache, &link);
        }
    }
    let launcher = scratch.join("ccache-launch");
    let script = format!(
        "#!/bin/sh\n# CMake compiler launcher: ccache, unless the compiler already is the ccache masquerade link.\ncase \"$(readlink \"$1\" 2>/dev/null)\" in *ccache) exec \"$@\";; esac\nexec '{}' \"$@\"\n",
        ccache.display()
    );
    if std::fs::write(&launcher, script).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755));
        let l = launcher.to_string_lossy().to_string();
        env.push(("CMAKE_C_COMPILER_LAUNCHER".to_string(), l.clone()));
        env.push(("CMAKE_CXX_COMPILER_LAUNCHER".to_string(), l));
    }
    let old_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    env.extend([
        ("PATH".to_string(), format!("{}:{old_path}", masq.display())),
        ("CCACHE_DIR".to_string(), scratch.join("ccache").to_string_lossy().to_string()),
        ("CCACHE_BASEDIR".to_string(), scratch.to_string_lossy().to_string()),
        ("CCACHE_NOHASHDIR".to_string(), "1".to_string()),
        ("CCACHE_MAXSIZE".to_string(), "2G".to_string()),
        ("CCACHE_SLOPPINESS".to_string(), "file_macro,time_macros".to_string()),
        ("CCACHE_COMPILERCHECK".to_string(), "content".to_string()),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_without_ccache_still_limits_jobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Job limits never depend on whether a ccache binary is found.
        let env = build_env(dir.path(), 3);
        assert!(env.contains(&("MAKEFLAGS".to_string(), "-j3".to_string())));
        assert!(env.contains(&("CMAKE_BUILD_PARALLEL_LEVEL".to_string(), "3".to_string())));
    }
}
