//! Installing from a bundle: every file verified before anything is used.

use std::path::Path;

use pretty_assertions::assert_eq;
use sha2::Digest as _;

use super::*;

/// A small bundle: two files, one link, a manifest.
fn bundle(dir: &Path) -> serde_json::Value {
    std::fs::create_dir_all(dir.join("python/bctx")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("python/bctx/cli.py"), "print('bctx')\n").unwrap();
    std::fs::write(dir.join("bin/clang-21"), "#!/bin/sh\necho clang\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("clang-21", dir.join("bin/clang")).unwrap();
    let entry = |rel: &str, mode: &str| {
        let bytes = std::fs::read(dir.join(rel)).unwrap();
        serde_json::json!({"sha256": hex(&sha2::Sha256::digest(&bytes)), "size": bytes.len(), "mode": mode})
    };
    let manifest = serde_json::json!({
        "schema": 1,
        "name": "byteask-context-engine",
        "version": "bctx test, LLVM 21.1.8",
        "files": {
            "python/bctx/cli.py": entry("python/bctx/cli.py", "644"),
            "bin/clang-21": entry("bin/clang-21", "755"),
            "bin/clang": {"link": "clang-21"},
        }
    });
    write_manifest(dir, &manifest);
    manifest
}

fn write_manifest(dir: &Path, manifest: &serde_json::Value) {
    std::fs::write(
        dir.join("MANIFEST.json"),
        serde_json::to_string_pretty(manifest).unwrap(),
    )
    .unwrap();
}

fn installs(home: &Path) -> Vec<String> {
    std::fs::read_dir(home.join(context_tools::engine::DIR_NAME))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_verified_bundle_installs_once() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    bundle(src.path());
    let first = install(home.path(), src.path()).expect("install");
    assert!(first.fresh);
    assert_eq!(first.version, "bctx test, LLVM 21.1.8");
    assert_eq!(
        std::fs::read_to_string(first.dir.join("python/bctx/cli.py")).unwrap(),
        "print('bctx')\n"
    );
    assert_eq!(
        std::fs::read_to_string(first.dir.join("serve.py")).unwrap(),
        SERVE_PY
    );
    assert_eq!(
        std::fs::read_to_string(first.dir.join(INDEX_DRIVER)).unwrap(),
        INDEX_DRIVER_PY
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(first.dir.join("bin/clang-21"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(first.dir.join("bin/clang")).unwrap(),
            Path::new("clang-21")
        );
    }
    let second = install(home.path(), src.path()).expect("again");
    assert!(
        !second.fresh,
        "an install that is already there is not copied again"
    );
    assert_eq!(second.dir, first.dir);
}

#[test]
fn a_changed_file_stops_the_install_and_leaves_nothing() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    bundle(src.path());
    std::fs::write(src.path().join("bin/clang-21"), "#!/bin/sh\necho evil\n").unwrap();
    let err = install(home.path(), src.path()).expect_err("refused");
    assert!(err.contains("does not match"), "{err}");
    assert!(
        installs(home.path()).is_empty(),
        "{:?}",
        installs(home.path())
    );
}

#[test]
fn a_missing_file_stops_the_install() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    bundle(src.path());
    std::fs::remove_file(src.path().join("python/bctx/cli.py")).unwrap();
    assert!(install(home.path(), src.path()).is_err());
    assert!(installs(home.path()).is_empty());
}

#[test]
fn a_path_or_link_that_leaves_the_bundle_is_refused() {
    for (rel, entry) in [
        (
            "../outside.py",
            serde_json::json!({"sha256": "00", "size": 0, "mode": "644"}),
        ),
        (
            "/etc/passwd",
            serde_json::json!({"sha256": "00", "size": 0, "mode": "644"}),
        ),
        ("bin/sh", serde_json::json!({"link": "../../../bin/sh"})),
        ("bin/up", serde_json::json!({"link": ".."})),
    ] {
        let src = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut manifest = bundle(src.path());
        manifest["files"][rel] = entry;
        write_manifest(src.path(), &manifest);
        assert!(
            install(home.path(), src.path()).is_err(),
            "{rel} was accepted"
        );
        assert!(installs(home.path()).is_empty(), "{rel}");
        assert!(!src.path().join("..").join("outside.py").exists());
    }
}

#[test]
fn the_bundle_is_found_next_to_the_running_byteask() {
    let root = tempfile::tempdir().unwrap();
    let ce = root.path().join("context-engine");
    std::fs::create_dir_all(&ce).unwrap();
    bundle(&ce);
    let exe = root.path().join("bin").join("byteask");
    let found = find_bundle(None, Some(&exe)).expect("found");
    assert_eq!(found, std::fs::canonicalize(&ce).unwrap());
    assert_eq!(
        find_bundle(Some(root.path()), None),
        Some(std::fs::canonicalize(&ce).unwrap())
    );
    assert_eq!(
        find_bundle(Some(&ce), None),
        Some(std::fs::canonicalize(&ce).unwrap())
    );
    assert_eq!(find_bundle(None, Some(Path::new("/usr/bin/byteask"))), None);
}

#[test]
fn python_is_checked_for_version_and_fts5() {
    match find_python(None) {
        Ok(python) => assert!(python.is_absolute()),
        Err(why) => assert!(why.contains("python3"), "{why}"),
    }
    let empty = tempfile::tempdir().unwrap();
    let err = find_python(Some(empty.path().as_os_str())).expect_err("no python");
    assert!(err.contains("not installed"), "{err}");
}
