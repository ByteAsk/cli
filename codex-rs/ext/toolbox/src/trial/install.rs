//! Installing the context engine from the trial bundle (no network).
//!
//! The bundle's `context-engine/MANIFEST.json` lists every file with its
//! sha256, size and mode. Every file is hashed while it is copied into a
//! fresh directory, and only a directory where every file matched is moved
//! into place (`$BYTEASK_HOME/context-engine/<id>`, id = the manifest's own
//! hash). A file that does not match, a path that leaves the bundle, or a link
//! that points outside its directory stops the install with nothing changed.

use std::io::Read as _;
use std::io::Write as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use sha2::Digest as _;
use sha2::Sha256;

/// The query server's launcher, written into every install.
pub const SERVE_PY: &str = include_str!("serve.py");
/// The one-target index driver, written into every install.
pub const INDEX_DRIVER_PY: &str = include_str!("index_driver.py");
/// Where the driver lives inside an install.
pub const INDEX_DRIVER: &str = "python/byteask_trial_index.py";
/// The largest manifest read (a manifest is a few hundred KB).
const MAX_MANIFEST_BYTES: u64 = 16 << 20;
/// Marker written last: an install without it is incomplete.
const DONE_MARKER: &str = ".installed";

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u32,
    version: String,
    files: std::collections::BTreeMap<String, Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Entry {
    Link {
        link: String,
    },
    File {
        sha256: String,
        size: u64,
        mode: String,
    },
}

/// An installed context engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub dir: PathBuf,
    pub version: String,
    /// Whether this run copied it (false = it was already installed).
    pub fresh: bool,
}

/// The bundle's `context-engine` directory: `--bundle` if given (the bundle
/// or its `context-engine` directory), else next to the running engine
/// (`<bundle>/bin/byteask` -> `<bundle>/context-engine`).
pub fn find_bundle(explicit: Option<&Path>, exe: Option<&Path>) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(dir) => vec![dir.join("context-engine"), dir.to_path_buf()],
        None => exe
            .and_then(Path::parent)
            .map(|bin| {
                let mut dirs = Vec::new();
                if let Some(top) = bin.parent() {
                    dirs.push(top.join("context-engine"));
                }
                dirs.push(bin.join("context-engine"));
                dirs
            })
            .unwrap_or_default(),
    };
    candidates
        .into_iter()
        .find(|dir| dir.join("MANIFEST.json").is_file())
        .map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A manifest path is relative, plain, and stays inside the bundle.
fn safe_relative(rel: &str) -> Option<PathBuf> {
    let path = Path::new(rel);
    if rel.is_empty() || path.is_absolute() {
        return None;
    }
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Some(path.to_path_buf())
    } else {
        None
    }
}

/// Install the bundle at `bundle` into `$BYTEASK_HOME/context-engine/`.
pub fn install(home: &Path, bundle: &Path) -> Result<Installed, String> {
    let manifest_path = bundle.join("MANIFEST.json");
    let mut raw = Vec::new();
    std::fs::File::open(&manifest_path)
        .and_then(|file| file.take(MAX_MANIFEST_BYTES).read_to_end(&mut raw))
        .map_err(|err| format!("could not read {}: {err}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&raw)
        .map_err(|err| format!("{} is not a valid manifest: {err}", manifest_path.display()))?;
    if manifest.schema != 1 {
        return Err(format!(
            "the bundle's manifest has format {}; this ByteAsk reads format 1",
            manifest.schema
        ));
    }
    let id: String = hex(&Sha256::digest(&raw))[..16].to_string();
    let root = home.join(context_tools::engine::DIR_NAME);
    context_tools::engine::create_private_dir(&root)
        .map_err(|err| format!("could not create {}: {err}", root.display()))?;
    let dest = root.join(&id);
    if dest.join(DONE_MARKER).is_file() {
        write_launchers(&dest)?;
        return Ok(Installed {
            dir: dest,
            version: manifest.version,
            fresh: false,
        });
    }
    let partial = root.join(format!("{id}.partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&partial);
    let result =
        copy_verified(bundle, &partial, &manifest).and_then(|()| write_launchers(&partial));
    if let Err(problem) = result {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(problem);
    }
    std::fs::write(partial.join(DONE_MARKER), format!("{id}\n"))
        .map_err(|err| format!("could not finish the install: {err}"))?;
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&partial, &dest)
        .map_err(|err| format!("could not finish the install: {err}"))?;
    Ok(Installed {
        dir: dest,
        version: manifest.version,
        fresh: true,
    })
}

fn copy_verified(bundle: &Path, dest: &Path, manifest: &Manifest) -> Result<(), String> {
    std::fs::create_dir_all(dest)
        .map_err(|err| format!("could not create {}: {err}", dest.display()))?;
    for (rel, entry) in &manifest.files {
        let rel_path = safe_relative(rel)
            .ok_or_else(|| format!("the manifest names an unsafe path: {rel}"))?;
        let target = dest.join(&rel_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        }
        match entry {
            Entry::Link { link } => {
                if link.contains('/') || link == "." || link == ".." || link.is_empty() {
                    return Err(format!(
                        "{rel}: a link may only name a file in its own directory"
                    ));
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(link, &target)
                    .map_err(|err| format!("could not create {}: {err}", target.display()))?;
                #[cfg(not(unix))]
                return Err("the context engine is supported on Linux only".to_string());
            }
            Entry::File { sha256, size, mode } => {
                let source = bundle.join(&rel_path);
                let meta = std::fs::symlink_metadata(&source)
                    .map_err(|_| format!("the bundle is missing {rel}"))?;
                if !meta.file_type().is_file() {
                    return Err(format!("{rel} in the bundle is not a regular file"));
                }
                let mut input = std::fs::File::open(&source)
                    .map_err(|err| format!("could not read {rel}: {err}"))?;
                let mut output = std::fs::File::create(&target)
                    .map_err(|err| format!("could not write {}: {err}", target.display()))?;
                let mut hasher = Sha256::new();
                let mut buf = vec![0u8; 1 << 20];
                let mut copied: u64 = 0;
                loop {
                    let n = input
                        .read(&mut buf)
                        .map_err(|err| format!("could not read {rel}: {err}"))?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    copied += n as u64;
                    output
                        .write_all(&buf[..n])
                        .map_err(|err| format!("could not write {}: {err}", target.display()))?;
                }
                if copied != *size || hex(&hasher.finalize()) != sha256.to_ascii_lowercase() {
                    return Err(format!(
                        "{rel} does not match the bundle's manifest (damaged or changed); get the bundle again"
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let bits = if mode == "755" { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(bits))
                        .map_err(|err| format!("could not set the mode of {rel}: {err}"))?;
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
        }
    }
    Ok(())
}

/// The two Python entry points ByteAsk owns, written fresh every run so they
/// always match this engine.
fn write_launchers(dir: &Path) -> Result<(), String> {
    for (rel, text) in [("serve.py", SERVE_PY), (INDEX_DRIVER, INDEX_DRIVER_PY)] {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        }
        std::fs::write(&path, text)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    }
    Ok(())
}

/// A Python 3.9+ whose sqlite has FTS5 (the index's word search needs it).
/// Returns the interpreter's absolute path.
pub fn find_python(path_var: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    let python = crate::trial::which("python3", path_var)
        .ok_or_else(|| "python3 is not installed (the context engine runs on it)".to_string())?;
    let probe = std::process::Command::new(&python)
        .args([
            "-I",
            "-c",
            "import sys, sqlite3\n\
             assert sys.version_info >= (3, 9), 'python ' + sys.version.split()[0]\n\
             sqlite3.connect(':memory:').execute('create virtual table t using fts5(x)')\n\
             print('ok')",
        ])
        .output()
        .map_err(|err| format!("could not run {}: {err}", python.display()))?;
    if probe.status.success() && String::from_utf8_lossy(&probe.stdout).trim() == "ok" {
        Ok(python)
    } else {
        let why = String::from_utf8_lossy(&probe.stderr);
        let last = why.lines().last().unwrap_or("").trim().to_string();
        Err(format!(
            "{} cannot run the context engine (needs Python 3.9+ with SQLite FTS5): {last}",
            python.display()
        ))
    }
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
