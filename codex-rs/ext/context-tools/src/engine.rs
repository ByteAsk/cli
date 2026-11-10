//! Where the context engine is installed and which projects it has indexed.
//!
//! One small file, `$BYTEASK_HOME/context-engine/engine.json`, written by
//! `byteask company trial` and read at every thread start:
//!
//! ```json
//! {"schema": 1, "version": "bctx 0.1 / LLVM 21.1.8", "install": "/home/me/.byteask/context-engine/<id>",
//!  "python": "/usr/bin/python3", "data": "/home/me/.byteask/context", "user": "local:me",
//!  "projects": [{"root": "/home/me/src/feed", "partition": "trial", "setting": "relwithdebinfo",
//!                "target": "feed_tests"}]}
//! ```
//!
//! The tools spawn `python` from this file, so it is read only when it is a
//! regular file owned by the current user and writable by nobody else (the
//! rule `ssh` applies to its own files). Anything else means "no context
//! engine here", and the tools are not offered.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Directory under `$BYTEASK_HOME` that holds installed context engines.
pub const DIR_NAME: &str = "context-engine";
/// The file this module reads and writes.
pub const FILE_NAME: &str = "engine.json";
/// Format of [`EngineFile`].
pub const SCHEMA: u32 = 1;

/// One indexed project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    /// The repository root the index was built from (canonical path).
    pub root: PathBuf,
    /// The index partition that holds it.
    pub partition: String,
    /// The build setting it was indexed in.
    pub setting: String,
    /// The CMake target whose build closure was indexed.
    pub target: String,
}

/// The whole file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineFile {
    pub schema: u32,
    /// Human-readable version of the installed context engine.
    pub version: String,
    /// The installed engine: `serve.py` and `python/` live here.
    pub install: PathBuf,
    /// The Python 3 interpreter that runs it.
    pub python: PathBuf,
    /// The index data root (partitions, overlays, access list).
    pub data: PathBuf,
    /// The identity every query is made as (the index's access list names it).
    pub user: String,
    pub projects: Vec<Project>,
}

/// The context engine as one thread sees it: the file plus the project the
/// thread works in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub engine: EngineFile,
    pub project: Project,
}

impl Workspace {
    /// The script that serves the context tools over JSON-RPC.
    pub fn serve_script(&self) -> PathBuf {
        self.engine.install.join("serve.py")
    }
}

/// `$BYTEASK_HOME/context-engine/engine.json`.
pub fn path(home: &Path) -> PathBuf {
    home.join(DIR_NAME).join(FILE_NAME)
}

/// Read the file. `Ok(None)` = no context engine installed; `Err` = a file
/// that exists but must not be used (the message says why, in plain words).
pub fn read(home: &Path) -> Result<Option<EngineFile>, String> {
    let file = path(home);
    let meta = match std::fs::symlink_metadata(&file) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{} could not be read: {err}", file.display())),
    };
    if !meta.file_type().is_file() {
        return Err(format!("{} is not a regular file", file.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: getuid has no preconditions and cannot fail.
        let me = unsafe { libc::getuid() };
        if meta.uid() != me {
            return Err(format!("{} belongs to another user", file.display()));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} can be changed by other users (run: chmod 600 {})",
                file.display(),
                file.display()
            ));
        }
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|err| format!("{} could not be read: {err}", file.display()))?;
    let parsed: EngineFile = serde_json::from_str(&text).map_err(|err| {
        format!(
            "{} is not a valid context engine record: {err}",
            file.display()
        )
    })?;
    if parsed.schema != SCHEMA {
        return Err(format!(
            "{} has format {} and this ByteAsk reads format {SCHEMA}",
            file.display(),
            parsed.schema
        ));
    }
    Ok(Some(parsed))
}

/// Write the file (0600, atomically).
pub fn write(home: &Path, engine: &EngineFile) -> io::Result<()> {
    let file = path(home);
    let dir = file.parent().unwrap_or(home);
    create_private_dir(dir)?;
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    let text = serde_json::to_string_pretty(engine).map_err(io::Error::other)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    {
        use std::io::Write as _;
        let mut out = options.open(&tmp)?;
        out.write_all(text.as_bytes())?;
        out.write_all(b"\n")?;
        out.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &file)
}

/// A directory only the user can enter (0700), created one level at a time
/// under an existing parent (never re-creating a missing `$BYTEASK_HOME`).
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(err) => return Err(err),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The indexed project `cwd` belongs to, if any (the deepest matching root).
pub fn project_for<'a>(engine: &'a EngineFile, cwd: &Path) -> Option<&'a Project> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    engine
        .projects
        .iter()
        .filter(|project| {
            let root =
                std::fs::canonicalize(&project.root).unwrap_or_else(|_| project.root.clone());
            cwd.starts_with(&root)
        })
        .max_by_key(|project| project.root.components().count())
}

/// The context engine for a thread working in `cwd`, if one has indexed it.
pub fn workspace_for(home: &Path, cwd: &Path) -> Option<Workspace> {
    let engine = read(home).ok().flatten()?;
    let project = project_for(&engine, cwd)?.clone();
    Some(Workspace { engine, project })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn sample(root: &Path) -> EngineFile {
        EngineFile {
            schema: SCHEMA,
            version: "test".to_string(),
            install: PathBuf::from("/opt/ctx"),
            python: PathBuf::from("/usr/bin/python3"),
            data: PathBuf::from("/data"),
            user: "local:me".to_string(),
            projects: vec![Project {
                root: root.to_path_buf(),
                partition: "trial".to_string(),
                setting: "relwithdebinfo".to_string(),
                target: "app".to_string(),
            }],
        }
    }

    #[test]
    fn written_file_reads_back_and_is_private() {
        let home = tempfile::tempdir().expect("home");
        let engine = sample(Path::new("/src/feed"));
        write(home.path(), &engine).expect("write");
        assert_eq!(read(home.path()).expect("read"), Some(engine));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(path(home.path()))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir = std::fs::metadata(home.path().join(DIR_NAME))
                .expect("dir")
                .permissions()
                .mode();
            assert_eq!(dir & 0o777, 0o700);
        }
    }

    #[test]
    fn no_file_means_no_context_engine() {
        let home = tempfile::tempdir().expect("home");
        assert_eq!(read(home.path()), Ok(None));
        assert_eq!(workspace_for(home.path(), Path::new("/")), None);
    }

    /// The tools start `python` from this file, so a file others can change
    /// is refused (as ssh refuses a group-writable config).
    #[cfg(unix)]
    #[test]
    fn a_file_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().expect("home");
        write(home.path(), &sample(Path::new("/src/feed"))).expect("write");
        std::fs::set_permissions(path(home.path()), std::fs::Permissions::from_mode(0o622))
            .expect("chmod");
        let err = read(home.path()).expect_err("refused");
        assert!(err.contains("other users"), "{err}");
        assert_eq!(workspace_for(home.path(), Path::new("/src/feed")), None);
    }

    #[test]
    fn an_unknown_format_is_refused() {
        let home = tempfile::tempdir().expect("home");
        let mut engine = sample(Path::new("/src/feed"));
        engine.schema = SCHEMA + 1;
        write(home.path(), &engine).expect("write");
        assert!(read(home.path()).is_err());
    }

    #[test]
    fn a_thread_gets_the_project_it_works_in_and_nothing_elsewhere() {
        let home = tempfile::tempdir().expect("home");
        let src = tempfile::tempdir().expect("src");
        let project = src.path().join("feed");
        let nested = project.join("src").join("book");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let other = src.path().join("feed-old");
        std::fs::create_dir_all(&other).expect("mkdir");
        let canonical = std::fs::canonicalize(&project).expect("canonical");
        write(home.path(), &sample(&canonical)).expect("write");
        let found = workspace_for(home.path(), &nested).expect("inside the project");
        assert_eq!(found.project.root, canonical);
        // A sibling whose name merely starts with the project's name is not inside it.
        assert_eq!(workspace_for(home.path(), &other), None);
        assert_eq!(workspace_for(home.path(), src.path()), None);
    }
}
