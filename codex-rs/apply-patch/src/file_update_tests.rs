use super::*;
use crate::Hunk;
use crate::apply_patch;
use crate::parse_patch;
use codex_exec_server::LOCAL_FS;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::tempdir;

fn wrap_patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

#[tokio::test]
async fn test_unified_diff() {
    // Start with a file containing four lines.
    let dir = tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    fs::write(&path, "foo\nbar\nbaz\nqux\n").unwrap();
    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
 foo
-bar
+BAR
@@
 baz
-qux
+QUX"#,
        path.display()
    ));
    let patch = parse_patch(&patch).unwrap();

    let update_file_chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };
    let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
    let diff = unified_diff_from_chunks(
        &path_uri,
        update_file_chunks,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .unwrap();
    let expected_diff = r#"@@ -1,4 +1,4 @@
 foo
-bar
+BAR
 baz
-qux
+QUX
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\nqux\n".to_string(),
        content: "foo\nBAR\nbaz\nQUX\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[tokio::test]
async fn test_unified_diff_first_line_replacement() {
    // Replace the very first line of the file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("first.txt");
    fs::write(&path, "foo\nbar\nbaz\n").unwrap();

    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
-foo
+FOO
 bar
"#,
        path.display()
    ));

    let patch = parse_patch(&patch).unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let resolved_path = PathUri::from_host_native_path(&path).expect("absolute test path");
    let diff = unified_diff_from_chunks(
        &resolved_path,
        chunks,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .unwrap();
    let expected_diff = r#"@@ -1,2 +1,2 @@
-foo
+FOO
 bar
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\n".to_string(),
        content: "FOO\nbar\nbaz\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[tokio::test]
async fn test_unified_diff_last_line_replacement() {
    // Replace the very last line of the file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("last.txt");
    fs::write(&path, "foo\nbar\nbaz\n").unwrap();

    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
 foo
 bar
-baz
+BAZ
"#,
        path.display()
    ));

    let patch = parse_patch(&patch).unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let resolved_path = PathUri::from_host_native_path(&path).expect("absolute test path");
    let diff = unified_diff_from_chunks(
        &resolved_path,
        chunks,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .unwrap();
    let expected_diff = r#"@@ -2,2 +2,2 @@
 bar
-baz
+BAZ
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\n".to_string(),
        content: "foo\nbar\nBAZ\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[tokio::test]
async fn test_unified_diff_insert_at_eof() {
    // Insert a new line at end-of-file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("insert.txt");
    fs::write(&path, "foo\nbar\nbaz\n").unwrap();

    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
+quux
*** End of File
"#,
        path.display()
    ));

    let patch = parse_patch(&patch).unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
    let diff =
        unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), /*sandbox*/ None)
            .await
            .unwrap();
    let expected_diff = r#"@@ -3 +3,2 @@
 baz
+quux
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\n".to_string(),
        content: "foo\nbar\nbaz\nquux\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[tokio::test]
async fn test_unified_diff_interleaved_changes() {
    // Original file with six lines.
    let dir = tempdir().unwrap();
    let path = dir.path().join("interleaved.txt");
    fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();

    // Patch replaces two separate lines and appends a new one at EOF using
    // three distinct chunks.
    let patch_body = format!(
        r#"*** Update File: {}
@@
 a
-b
+B
@@
 d
-e
+E
@@
 f
+g
*** End of File"#,
        path.display()
    );
    let patch = wrap_patch(&patch_body);

    // Extract chunks then build the unified diff.
    let parsed = parse_patch(&patch).unwrap();
    let chunks = match parsed.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
    let diff =
        unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), /*sandbox*/ None)
            .await
            .unwrap();

    let expected_diff = r#"@@ -1,6 +1,7 @@
 a
-b
+B
 c
 d
-e
+E
 f
+g
"#;

    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "a\nb\nc\nd\ne\nf\n".to_string(),
        content: "a\nB\nc\nd\nE\nf\ng\n".to_string(),
    };

    assert_eq!(expected, diff);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    apply_patch(
        &patch,
        &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
        &mut stdout,
        &mut stderr,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .unwrap();
    let contents = fs::read_to_string(path).unwrap();
    assert_eq!(
        contents,
        r#"a
B
c
d
E
f
g
"#
    );
}

// ---- ByteAsk: strict_patch refuses hunks whose position is a guess ----------------------

async fn strict_check(dir: &std::path::Path, body: &str) -> Result<(), String> {
    let args = parse_patch(&wrap_patch(body)).expect("patch parses");
    let cwd = PathUri::from_host_native_path(dir).expect("absolute test dir");
    for mode in [
        ApplyPatchFileUpdateMode::NormalizeToLf,
        ApplyPatchFileUpdateMode::PreserveLineEndings,
    ] {
        crate::check_unambiguous_hunks(&args, &cwd, mode, LOCAL_FS.as_ref(), /*sandbox*/ None)
            .await
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[tokio::test]
async fn strict_refuses_lines_that_occur_twice() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("a.cpp"),
        "void f() {\n  x = 0;\n}\nvoid g() {\n  x = 0;\n}\n",
    )
    .unwrap();
    let err = strict_check(
        dir.path(),
        "*** Update File: a.cpp\n@@\n-  x = 0;\n+  x = 1;",
    )
    .await
    .expect_err("two places match");
    assert!(err.contains("at line 2 and line 5"), "{err}");
    assert!(err.contains("Nothing was changed"), "{err}");
    // the file is untouched
    assert_eq!(
        fs::read_to_string(dir.path().join("a.cpp")).unwrap(),
        "void f() {\n  x = 0;\n}\nvoid g() {\n  x = 0;\n}\n"
    );
}

#[tokio::test]
async fn strict_accepts_a_unique_match_an_anchor_or_more_context() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("a.cpp"),
        "void f() {\n  x = 0;\n}\nvoid g() {\n  x = 0;\n}\n",
    )
    .unwrap();
    // an @@ line narrows the search: the first match after it is the one meant
    strict_check(
        dir.path(),
        "*** Update File: a.cpp\n@@ void g() {\n-  x = 0;\n+  x = 1;",
    )
    .await
    .expect("anchored");
    // enough context to be unique
    strict_check(
        dir.path(),
        "*** Update File: a.cpp\n@@\n void g() {\n-  x = 0;\n+  x = 1;",
    )
    .await
    .expect("unique with context");
    // later chunks search after the earlier ones: two chunks, one per occurrence, in order
    strict_check(
        dir.path(),
        "*** Update File: a.cpp\n@@\n void f() {\n-  x = 0;\n+  x = 1;\n@@\n void g() {\n-  x = 0;\n+  x = 2;",
    )
    .await
    .expect("one chunk per place");
}

#[tokio::test]
async fn strict_refuses_an_insertion_without_context_but_allows_end_of_file() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.cpp"), "int a;\nint b;\n").unwrap();
    let err = strict_check(dir.path(), "*** Update File: a.cpp\n@@ int a;\n+int c;")
        .await
        .expect_err("no line to match");
    assert!(err.contains("appended at the end of the file"), "{err}");
    strict_check(
        dir.path(),
        "*** Update File: a.cpp\n@@\n+int c;\n*** End of File",
    )
    .await
    .expect("explicit end of file");
    // an empty file has only one place to insert
    fs::write(dir.path().join("empty.cpp"), "").unwrap();
    strict_check(dir.path(), "*** Update File: empty.cpp\n@@\n+int c;")
        .await
        .expect("empty file");
}

/// Without strict mode the same patch applies to the first occurrence, as upstream does.
#[tokio::test]
async fn without_strict_the_first_occurrence_is_changed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.cpp");
    fs::write(&path, "  x = 0;\n}\n  x = 0;\n").unwrap();
    let patch = parse_patch(&wrap_patch(&format!(
        "*** Update File: {}\n@@\n-  x = 0;\n+  x = 1;",
        path.display()
    )))
    .unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("one update"),
    };
    let path_uri = PathUri::from_host_native_path(&path).unwrap();
    let diff = unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), None)
        .await
        .unwrap();
    assert_eq!(diff.content, "  x = 1;\n}\n  x = 0;\n");
}
