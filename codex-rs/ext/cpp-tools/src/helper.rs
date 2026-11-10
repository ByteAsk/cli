//! Materialize the engine-owned `byteask_gdb_bridge.py` into the user's config
//! directory on startup.
//!
//! The helper script is embedded into the binary at compile time (source of
//! truth: `ext/cpp-tools/byteask_gdb_bridge.py`). On startup we write it to
//! `<codex_home>/byteask_gdb_bridge.py`, overwriting any on-disk copy that differs
//! from the embedded bytes. This is best-effort: every failure is swallowed so a
//! read-only home, missing permissions, or a clock error can never break a
//! session (mirrors `core/src/installation_id.rs` and `report.rs`).

use std::path::Path;

/// The engine-owned helper script, embedded at compile time. Path is relative to
/// this source file (`src/helper.rs` → `../byteask_gdb_bridge.py`).
const HELPER_PY: &str = include_str!("../byteask_gdb_bridge.py");

/// File name written under `codex_home`.
const HELPER_FILENAME: &str = "byteask_gdb_bridge.py";

/// Write the embedded helper to `<codex_home>/byteask_gdb_bridge.py` if it is
/// missing or its bytes differ from the embedded copy. Best-effort: all errors
/// are swallowed (never panics).
pub(crate) fn materialize_helper(codex_home: &Path) {
    // Legacy cleanup: the helper was renamed `cpp_tools_helper.py` -> the current
    // HELPER_FILENAME. Remove the stale copy so it doesn't linger in ~/.byteask.
    // Best-effort — ignored if absent or unwritable.
    let _ = std::fs::remove_file(codex_home.join("cpp_tools_helper.py"));

    let target = codex_home.join(HELPER_FILENAME);

    // Skip the write only when the on-disk bytes already match exactly.
    if let Ok(existing) = std::fs::read(&target) {
        if existing == HELPER_PY.as_bytes() {
            return;
        }
    }

    let _ = std::fs::create_dir_all(codex_home);
    let _ = std::fs::write(&target, HELPER_PY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(HELPER_FILENAME);
        assert!(!target.exists());
        materialize_helper(dir.path());
        assert_eq!(std::fs::read(&target).unwrap(), HELPER_PY.as_bytes());
    }

    #[test]
    fn removes_legacy_cpp_tools_helper() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpp_tools_helper.py"), b"stale legacy").unwrap();
        materialize_helper(dir.path());
        assert!(
            !dir.path().join("cpp_tools_helper.py").exists(),
            "legacy cpp_tools_helper.py should be removed"
        );
        assert!(
            dir.path().join(HELPER_FILENAME).exists(),
            "new bridge written"
        );
    }

    #[test]
    fn no_rewrite_when_identical() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(HELPER_FILENAME);
        std::fs::write(&target, HELPER_PY).unwrap();
        let mtime_before = std::fs::metadata(&target).unwrap().modified().unwrap();
        // Ensure a measurable gap so a rewrite would change mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        materialize_helper(dir.path());
        let mtime_after = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "identical content must not be rewritten"
        );
    }

    #[test]
    fn overwrites_when_differs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(HELPER_FILENAME);
        std::fs::write(&target, b"stale local edits").unwrap();
        materialize_helper(dir.path());
        assert_eq!(std::fs::read(&target).unwrap(), HELPER_PY.as_bytes());
    }

    #[test]
    fn creates_dir_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        assert!(!nested.exists());
        materialize_helper(&nested);
        let target = nested.join(HELPER_FILENAME);
        assert_eq!(std::fs::read(&target).unwrap(), HELPER_PY.as_bytes());
    }

    #[test]
    fn written_content_byte_equals_embedded() {
        let dir = tempfile::tempdir().unwrap();
        materialize_helper(dir.path());
        let written = std::fs::read(dir.path().join(HELPER_FILENAME)).unwrap();
        assert_eq!(written, HELPER_PY.as_bytes());
    }

    #[test]
    #[cfg(unix)]
    fn unwritable_dir_does_not_panic() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Make the directory read-only so create_dir_all/write both fail.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        let target = dir.path().join("ro").join("c");
        // Must not panic even though the write cannot succeed.
        materialize_helper(&target);
        // Restore perms so tempdir cleanup can remove it.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(dir.path(), perms).unwrap();
    }
}
