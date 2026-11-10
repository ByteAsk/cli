use pretty_assertions::assert_eq;

use super::*;

const FULL: &str = r#"
schema = 1

[build]
run = ["cmake --preset ci", "cmake --build --preset ci"]
timeout_secs = 1800

[test]
run = ["ctest --preset ci --output-junit \"$BYTEASK_RESULTS_DIR/junit.xml\""]
junit = "junit.xml"
manifest = ".byteask/required-tests.txt"

[[slow]]
name = "asan"
run = ["cmake --preset asan && cmake --build --preset asan && ctest --preset asan"]

[files]
marked = ["src/feed/**"]

[merge]
reviewer_may_approve = ["no_code_change", "time_limit"]
perf = "required"
"#;

#[test]
fn a_full_profile_parses() {
    let profile = Profile::parse(FULL).expect("parses");
    assert_eq!(profile.build.as_ref().map(|b| b.run.len()), Some(2));
    let test = profile.test.as_ref().expect("test");
    assert_eq!(test.manifest.as_deref(), Some(".byteask/required-tests.txt"));
    assert_eq!(profile.slow[0].name, "asan");
    assert_eq!(
        profile.merge.reviewer_may_approve,
        vec![NotRunReason::NoCodeChange, NotRunReason::TimeLimit]
    );
    assert_eq!(profile.merge.perf, PerfPolicy::Required);
}

#[test]
fn defaults_apply_when_sections_are_missing() {
    let profile = Profile::parse("schema = 1\n[build]\nrun = [\"make\"]\n").expect("parses");
    assert_eq!(profile.merge.reviewer_may_approve, vec![NotRunReason::NoCodeChange]);
    assert_eq!(profile.merge.perf, PerfPolicy::Off);
    assert!(profile.test.is_none());
}

/// A typo must never switch a check off silently.
#[test]
fn bad_profiles_are_refused_with_a_reason() {
    let cases = [
        ("schema = 2\n", "schema 2"),
        ("schema = 1\n[build]\nrun = []\n", "at least one"),
        ("schema = 1\n[build]\nrun = [\"  \"]\n", "at least one"),
        ("schema = 1\n[build]\nrun = [\"make\"]\ntimout_secs = 5\n", "unknown field"),
        ("schema = 1\n[biuld]\nrun = [\"make\"]\n", "unknown field"),
        ("schema = 1\n[build]\nrun = [\"make\"]\ntimeout_secs = 0\n", "timeout_secs"),
        ("schema = 1\n[[slow]]\nname = \"Bad Name\"\nrun = [\"x\"]\n", "slow check name"),
        ("schema = 1\n[[slow]]\nname = \"tests\"\nrun = [\"x\"]\n", "reserved"),
        (
            "schema = 1\n[[slow]]\nname = \"a\"\nrun = [\"x\"]\n[[slow]]\nname = \"a\"\nrun = [\"y\"]\n",
            "used twice",
        ),
        ("schema = 1\n[test]\nrun = [\"x\"]\nmanifest = \"../outside\"\n", "relative path"),
        ("schema = 1\n[test]\nrun = [\"x\"]\njunit = \"/abs/junit.xml\"\n", "relative path"),
        ("schema = 1\n[merge]\nreviewer_may_approve = [\"not_checked\"]\n", "not_checked"),
        ("schema = 1\n[merge]\nreviewer_may_approve = [\"made_up\"]\n", "unknown variant"),
        ("schema = 1\n[merge]\nperf = \"sometimes\"\n", "unknown variant"),
        ("schema = 1\n[files]\nmarked = [\"src/[\"]\n", "bad file pattern"),
    ];
    for (text, wanted) in cases {
        let err = Profile::parse(text).expect_err(text);
        assert!(err.contains(wanted), "{text:?} -> {err}");
    }
}

/// Only the explicit non-code list is "no code or build change"; everything else,
/// unknown types included, is code.
#[test]
fn default_file_classes() {
    let classes = FileClasses::defaults();
    for path in [
        "README.md",
        "docs/guide.rst",
        "doc/api/notes.txt",
        "sub/docs/x.txt",
        "LICENSE",
        "LICENSE.txt",
        "third_party/foo/COPYING",
        "CHANGELOG.md",
        "assets/logo.png",
        "img/diagram.SVG",
        "./notes.markdown",
    ] {
        assert!(!classes.is_code(path), "{path} should be non-code");
    }
    for path in [
        "src/book.cpp",
        "include/book.h",
        "CMakeLists.txt",
        "src/CMakeLists.txt",
        "cmake/Warnings.cmake",
        ".byteask/checks.toml",
        ".byteask/required-tests.txt",
        ".byteask/README.md",
        "version.txt",
        "tests/data/expected.txt",
        "conanfile.txt",
        "requirements-dev.txt",
        "Makefile",
        "data/feed.bin",
        "something.unknownext",
    ] {
        assert!(classes.is_code(path), "{path} should be code");
    }
}

#[test]
fn only_non_code_needs_every_path_and_at_least_one() {
    let classes = FileClasses::defaults();
    assert!(classes.only_non_code(["README.md", "docs/a.rst"]));
    assert!(!classes.only_non_code(["README.md", "src/a.cpp"]));
    assert!(!classes.only_non_code(std::iter::empty::<&str>()));
}

#[test]
fn a_profile_can_replace_non_code_but_never_unprotect_the_profile() {
    let files = Files {
        non_code: Some(vec!["**/*.txt".to_string(), "**/*.toml".to_string()]),
        code: Some(vec![]),
        marked: vec!["src/hot/**".to_string()],
    };
    let classes = FileClasses::new(&files).expect("classes");
    assert!(!classes.is_code("tests/data/expected.txt"));
    assert!(classes.is_code("README.md"), "the default list was replaced");
    assert!(classes.is_code(".byteask/checks.toml"), "ALWAYS_CODE");
    assert!(classes.is_code("CMakeLists.txt"), "ALWAYS_CODE");
    assert!(classes.touches_marked(["docs/x.md", "src/hot/feed.cpp"]));
    assert!(!classes.touches_marked(["src/cold/feed.cpp"]));
}

#[test]
fn propose_reads_cmake_presets_first() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("CMakeLists.txt"), "project(x)\n").expect("write");
    std::fs::write(
        dir.path().join("CMakePresets.json"),
        r#"{"version": 3,
            "configurePresets": [{"name": "base", "hidden": true}, {"name": "dev"}, {"name": "ci"}],
            "buildPresets": [{"name": "ci"}],
            "testPresets": [{"name": "ci"}, {"name": "bad name; rm -rf /"}]}"#,
    )
    .expect("write");
    let proposal = propose(dir.path());
    assert!(proposal.source.contains("CMakePresets.json"), "{proposal:?}");
    let profile = Profile::parse(&proposal.text).expect("the proposal itself parses");
    assert_eq!(
        profile.build.expect("build").run,
        vec!["cmake --preset ci".to_string(), "cmake --build --preset ci".to_string()]
    );
    assert!(profile.test.expect("test").run[0].starts_with("ctest --preset ci --output-junit"));
}

#[test]
fn propose_uses_a_compile_database_build_tree_then_cmakelists() {
    let dir = tempfile::tempdir().expect("tmp");
    let tree = dir.path().join("build-rel");
    std::fs::create_dir(&tree).expect("mkdir");
    std::fs::write(tree.join("compile_commands.json"), "[]").expect("write");
    std::fs::write(tree.join("CMakeCache.txt"), "").expect("write");
    let proposal = propose(dir.path());
    let profile = Profile::parse(&proposal.text).expect("parses");
    assert_eq!(profile.build.expect("build").run, vec!["cmake --build build-rel".to_string()]);

    let plain = tempfile::tempdir().expect("tmp");
    std::fs::write(plain.path().join("CMakeLists.txt"), "project(x)\n").expect("write");
    let profile = Profile::parse(&propose(plain.path()).text).expect("parses");
    assert!(profile.build.expect("build").run[0].starts_with("cmake -S . -B build/byteask-ci"));
}

/// An unedited "nothing found" proposal must not parse: a placeholder build command that
/// always succeeds would read as a PASS.
#[test]
fn propose_with_nothing_found_does_not_parse_until_filled_in() {
    let dir = tempfile::tempdir().expect("tmp");
    let proposal = propose(dir.path());
    assert!(proposal.source.starts_with("nothing found"), "{proposal:?}");
    let err = Profile::parse(&proposal.text).expect_err("placeholder profile");
    assert!(err.contains("at least one"), "{err}");
}
