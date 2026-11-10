//! Patch helpers for Tier B: split a candidate's change into the fix and the tests it added,
//! size measures, a tree for "snapshot + patch" built without a work tree, and a cap on the
//! build parallelism of engine-run commands.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

use regex_lite::Regex;

use crate::attempts::testparse::is_test_path;
use crate::gitops::ChangeKind;
use crate::runner::spawn_capture;

/// One `diff --git` section of a patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub path: String,
    pub kind: ChangeKind,
    pub text: Vec<u8>,
    /// The section only adds lines (or adds the file).
    pub adds_only: bool,
    pub added_lines: Vec<String>,
}

/// Split a git patch into its per-file sections (a line starting with `diff --git ` always
/// starts a section: hunk lines carry a one-character prefix and binary data has no spaces).
pub fn sections(patch: &[u8]) -> Vec<Section> {
    let text = String::from_utf8_lossy(patch);
    let mut out: Vec<Section> = Vec::new();
    let mut in_hunk = false;
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let rest = rest.trim_end();
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            out.push(Section {
                path,
                kind: ChangeKind::Modified,
                text: Vec::new(),
                adds_only: true,
                added_lines: Vec::new(),
            });
            in_hunk = false;
        }
        let Some(cur) = out.last_mut() else {
            continue;
        };
        cur.text.extend_from_slice(line.as_bytes());
        if !in_hunk {
            if line.starts_with("new file mode") {
                cur.kind = ChangeKind::Added;
            } else if line.starts_with("deleted file mode") {
                cur.kind = ChangeKind::Deleted;
                cur.adds_only = false;
            } else if line.starts_with("GIT binary patch") {
                cur.adds_only = cur.kind == ChangeKind::Added;
            } else if line.starts_with("@@") {
                in_hunk = true;
            }
            continue;
        }
        if line.starts_with("@@") {
            continue;
        }
        if let Some(body) = line.strip_prefix('+') {
            cur.added_lines.push(body.trim_end().to_string());
        } else if line.starts_with('-') {
            cur.adds_only = false;
        }
    }
    out
}

fn is_build_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "CMakeLists.txt"
        || name.ends_with(".cmake")
        || name.starts_with("Makefile")
        || name == "meson.build"
        || name.ends_with(".am")
        || name.ends_with(".mk")
}

/// Split a candidate's change into (fix, tests). Test sections: new test files, test files that
/// only gain lines, and build files that only gain lines naming one of those test files (their
/// registration). Everything else, including edits to existing tests, stays in the fix.
pub fn split_tests(patch: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let secs = sections(patch);
    let is_test = |s: &Section| {
        is_test_path(&s.path)
            && (s.kind == ChangeKind::Added || (s.kind == ChangeKind::Modified && s.adds_only))
    };
    let stems: Vec<String> = secs
        .iter()
        .filter(|s| is_test(s))
        .filter_map(|s| {
            let name = s.path.rsplit('/').next()?;
            let stem = name.split('.').next()?;
            (stem.len() >= 3).then(|| stem.to_string())
        })
        .collect();
    let registers_tests = |s: &Section| {
        is_build_file(&s.path)
            && !is_test_path(&s.path)
            && s.kind == ChangeKind::Modified
            && s.adds_only
            && !stems.is_empty()
            && s
                .added_lines
                .iter()
                .any(|l| stems.iter().any(|stem| l.contains(stem.as_str())))
    };
    let mut fix = Vec::new();
    let mut tests = Vec::new();
    for s in &secs {
        if is_test(s) || registers_tests(s) {
            tests.extend_from_slice(&s.text);
        } else {
            fix.extend_from_slice(&s.text);
        }
    }
    (fix, tests)
}

/// `patch` without the sections for `paths`.
pub fn drop_paths(patch: &[u8], paths: &BTreeSet<String>) -> Vec<u8> {
    if paths.is_empty() {
        return patch.to_vec();
    }
    sections(patch)
        .into_iter()
        .filter(|s| !paths.contains(&s.path))
        .flat_map(|s| s.text)
        .collect()
}

/// Paths the snapshot added on top of HEAD: files that were untracked when the session started.
/// Changes to them are build noise that a patch of the session's changes leaves out, so Tier B
/// leaves them out of candidate changes and of the final switch too.
pub async fn added_by_snapshot(repo: &Path, snapshot_commit: &str) -> BTreeSet<String> {
    let parent = format!("{snapshot_commit}^");
    let (ok, out, _) = spawn_capture(
        "git",
        &["-c", "core.quotepath=off", "diff-tree", "-r", "-z", "--no-commit-id", "--name-only", "--diff-filter=A", &parent, snapshot_commit],
        repo,
        &[("GIT_OPTIONAL_LOCKS", "0")],
        None,
        std::time::Duration::from_secs(120),
    )
    .await;
    if !ok {
        return BTreeSet::new();
    }
    String::from_utf8_lossy(&out)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Added plus removed lines (hunk bodies only).
pub fn changed_lines(patch: &[u8]) -> usize {
    sections(patch)
        .iter()
        .map(|s| {
            let text = String::from_utf8_lossy(&s.text);
            let mut in_hunk = false;
            text.lines()
                .filter(|l| {
                    if l.starts_with("@@") {
                        in_hunk = true;
                        return false;
                    }
                    in_hunk && (l.starts_with('+') || l.starts_with('-'))
                })
                .count()
        })
        .sum()
}

pub fn changed_files(patch: &[u8]) -> usize {
    sections(patch).len()
}

async fn git_with_index(repo: &Path, index: &str, args: &[&str], stdin: Option<Vec<u8>>) -> Result<String, String> {
    let mut full = vec!["-c", "core.hooksPath=/dev/null", "-c", "safe.directory=*"];
    full.extend_from_slice(args);
    let env = [("GIT_INDEX_FILE", index), ("GIT_OPTIONAL_LOCKS", "0")];
    let (ok, out, err) = spawn_capture("git", &full, repo, &env, stdin, std::time::Duration::from_secs(300)).await;
    if ok {
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    } else {
        Err(err.trim().to_string())
    }
}

/// Tree object for `base_tree` plus `patch`, built in a temporary index (no work tree touched).
pub async fn tree_with_patch(repo: &Path, base_tree: &str, patch: &[u8], tmp: &Path) -> Result<String, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = tmp.join(format!("index-tierb-{}-{seq}", std::process::id()));
    let index_s = index.to_string_lossy().to_string();
    let result = async {
        git_with_index(repo, &index_s, &["read-tree", base_tree], None).await?;
        if !patch.is_empty() {
            git_with_index(
                repo,
                &index_s,
                &["apply", "--cached", "--binary", "--whitespace=nowarn", "-"],
                Some(patch.to_vec()),
            )
            .await?;
        }
        git_with_index(repo, &index_s, &["write-tree"], None).await
    }
    .await;
    let _ = std::fs::remove_file(&index);
    result
}

#[allow(clippy::expect_used)] // constant pattern, covered by the unit tests
static TOKENS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+|\s+").expect("valid regex"));

/// Cap the build/test parallelism a command asks for at `jobs`: `-j`, `-jN`, `-j N`,
/// `--parallel [N]`, `--parallel=N` (N = 0 means unbounded for some tools). Engine-run builds
/// happen one at a time; this keeps each of them inside the memory the session has.
pub fn cap_parallelism(cmd: &str, jobs: usize) -> String {
    let toks: Vec<&str> = TOKENS.find_iter(cmd).map(|m| m.as_str()).collect();
    let too_many = |n: &str| n.parse::<usize>().is_ok_and(|n| n == 0 || n > jobs);
    // A count computed by the shell (`$(nproc)`, a variable) is replaced too.
    let computed = |w: &str| w.starts_with('$') || w.starts_with('`');
    let next_word = |i: usize| toks.get(i + 1).and_then(|_| toks.get(i + 2)).copied();
    let mut out = String::with_capacity(cmd.len() + 4);
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t == "-j" || t == "--parallel" {
            match next_word(i) {
                Some(n) if n.chars().all(|c| c.is_ascii_digit()) || computed(n) => {
                    out.push_str(t);
                    out.push_str(toks[i + 1]);
                    out.push_str(&if computed(n) || too_many(n) { jobs.to_string() } else { n.to_string() });
                    i += 3;
                    continue;
                }
                _ => {
                    out.push_str(&format!("{t} {jobs}").replace("-j ", "-j"));
                    i += 1;
                    continue;
                }
            }
        }
        if let Some(n) = t.strip_prefix("-j")
            && !n.is_empty()
            && (n.chars().all(|c| c.is_ascii_digit()) || computed(n))
        {
            out.push_str(&if computed(n) || too_many(n) { format!("-j{jobs}") } else { t.to_string() });
        } else if let Some(n) = t.strip_prefix("--parallel=") {
            out.push_str(&if computed(n) || too_many(n) { format!("--parallel={jobs}") } else { t.to_string() });
        } else {
            out.push_str(t);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const PATCH: &str = "\
diff --git a/src/calc.cpp b/src/calc.cpp
index 1..2 100644
--- a/src/calc.cpp
+++ b/src/calc.cpp
@@ -1,3 +1,3 @@
 int add(int a, int b) {
-  return a - b;
+  return a + b;
 }
diff --git a/test/add_case.cpp b/test/add_case.cpp
new file mode 100644
--- /dev/null
+++ b/test/add_case.cpp
@@ -0,0 +1 @@
+int main() { return 0; }
diff --git a/CMakeLists.txt b/CMakeLists.txt
index 3..4 100644
--- a/CMakeLists.txt
+++ b/CMakeLists.txt
@@ -9,0 +10,2 @@
+add_executable(add_case test/add_case.cpp)
+add_test(NAME add_case COMMAND add_case)
diff --git a/test/old_test.cpp b/test/old_test.cpp
index 5..6 100644
--- a/test/old_test.cpp
+++ b/test/old_test.cpp
@@ -1 +1 @@
-  CHECK(f() == 1);
+  CHECK(f() == 2);
";

    #[test]
    fn sections_and_sizes() {
        let secs = sections(PATCH.as_bytes());
        let paths: Vec<&str> = secs.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["src/calc.cpp", "test/add_case.cpp", "CMakeLists.txt", "test/old_test.cpp"]);
        assert_eq!(secs[1].kind, ChangeKind::Added);
        assert!(secs[2].adds_only && !secs[0].adds_only && !secs[3].adds_only);
        assert_eq!(changed_files(PATCH.as_bytes()), 4);
        assert_eq!(changed_lines(PATCH.as_bytes()), 2 + 1 + 2 + 2);
        let joined: Vec<u8> = secs.iter().flat_map(|s| s.text.clone()).collect();
        assert_eq!(joined, PATCH.as_bytes());
    }

    #[test]
    fn new_tests_and_their_registration_leave_the_fix() {
        let (fix, tests) = split_tests(PATCH.as_bytes());
        let fix = String::from_utf8_lossy(&fix).to_string();
        let tests = String::from_utf8_lossy(&tests).to_string();
        assert!(fix.contains("src/calc.cpp") && fix.contains("test/old_test.cpp"), "{fix}");
        assert!(!fix.contains("add_case"), "{fix}");
        assert!(tests.contains("b/test/add_case.cpp") && tests.contains("add_test(NAME add_case"), "{tests}");
        assert!(!tests.contains("old_test"), "an edit to an existing test stays in the fix: {tests}");
        // Nothing to split.
        let (fix, tests) = split_tests(b"");
        assert!(fix.is_empty() && tests.is_empty());
    }

    #[test]
    fn paths_are_dropped() {
        let keep: BTreeSet<String> = ["CMakeLists.txt".to_string(), "test/add_case.cpp".to_string()].into();
        let out = String::from_utf8_lossy(&drop_paths(PATCH.as_bytes(), &keep)).to_string();
        let paths: Vec<String> = sections(out.as_bytes()).into_iter().map(|s| s.path).collect();
        assert_eq!(paths, vec!["src/calc.cpp".to_string(), "test/old_test.cpp".to_string()]);
        assert_eq!(drop_paths(PATCH.as_bytes(), &BTreeSet::new()), PATCH.as_bytes());
    }

    #[test]
    fn parallelism_is_capped() {
        assert_eq!(cap_parallelism("make -j", 2), "make -j2");
        assert_eq!(cap_parallelism("make -j16 all", 2), "make -j2 all");
        assert_eq!(cap_parallelism("make -j1 all", 2), "make -j1 all");
        assert_eq!(cap_parallelism("cmake --build b -j 8 && ctest -j0", 2), "cmake --build b -j 2 && ctest -j2");
        assert_eq!(cap_parallelism("cmake --build b --parallel", 4), "cmake --build b --parallel 4");
        assert_eq!(cap_parallelism("cmake --build b --parallel 3", 4), "cmake --build b --parallel 3");
        assert_eq!(cap_parallelism("ninja --parallel=12", 4), "ninja --parallel=4");
        assert_eq!(cap_parallelism("echo 'a  b' && make", 2), "echo 'a  b' && make");
        assert_eq!(cap_parallelism("make -j $(nproc) && make -j`nproc` x", 2), "make -j 2 && make -j2 x");
        assert_eq!(cap_parallelism("java -jar t.jar", 2), "java -jar t.jar");
    }

    #[tokio::test]
    async fn tree_with_patch_needs_no_work_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "x"]);
        let base = run(&["rev-parse", "HEAD^{tree}"]);
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
        let tree = tree_with_patch(repo, &base, patch.as_bytes(), &repo.join(".git/tmp-test"))
            .await
            .expect("tree");
        assert_ne!(tree, base);
        assert_eq!(run(&["cat-file", "-p", &format!("{tree}:a.txt")]), "two");
        // The work tree and the real index are untouched.
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).expect("read"), "one\n");
        assert_eq!(run(&["status", "--porcelain"]), "");
        // A snapshot commit that adds an untracked file on top of HEAD.
        let head = run(&["rev-parse", "HEAD"]);
        let added = "diff --git a/gen.txt b/gen.txt\nnew file mode 100644\n--- /dev/null\n+++ b/gen.txt\n@@ -0,0 +1 @@\n+x\n";
        let snap_tree = tree_with_patch(repo, &base, added.as_bytes(), &repo.join(".git/tmp-test"))
            .await
            .expect("tree");
        let snap = run(&["commit-tree", &snap_tree, "-p", &head, "-m", "snap"]);
        let noise = added_by_snapshot(repo, &snap).await;
        assert_eq!(noise, ["gen.txt".to_string()].into());
    }
}
