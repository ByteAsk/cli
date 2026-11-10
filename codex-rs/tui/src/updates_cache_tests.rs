use super::*;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn cache_file_is_byteask_named_and_tolerates_missing_feed_fields() {
    assert!(
        VERSION_FILENAME.starts_with("byteask"),
        "must not reuse upstream's version.json"
    );
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join(VERSION_FILENAME);
    std::fs::write(&path, r#"{"last_checked_at":"2026-09-14T00:00:00Z"}"#).expect("write cache");

    let info = read_version_info(&path).expect("read cache");
    assert_eq!((info.latest_version, info.min_supported), (None, None));
}
