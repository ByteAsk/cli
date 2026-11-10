//! What a shell command the agent ran was for (configure, build, test, lint), and where it
//! wrote JUnit XML. Heuristics on the command text only: nothing is executed here.

use std::path::Path;
use std::path::PathBuf;

use crate::Purpose;

const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh"];

/// The script inside a shell wrapper: the tools report commands as an argv joined for display,
/// e.g. `/bin/bash -lc 'cmake --build build'`; verify re-runs the inner script with `bash -lc`.
pub(crate) fn unwrap_shell(command: &str) -> String {
    let Some(argv) = shlex::split(command) else {
        return command.trim().to_string();
    };
    if let Some(script) = shell_script(&argv) {
        return script.trim().to_string();
    }
    command.trim().to_string()
}

/// `[bash, -lc, SCRIPT]` (or `-c`, `-l -c`, ...) to `SCRIPT`.
fn shell_script(argv: &[String]) -> Option<String> {
    let program = basename(argv.first()?);
    if !SHELLS.contains(&program) {
        return None;
    }
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" || !arg.starts_with('-') {
            return None;
        }
        if arg.contains('c') && !arg.starts_with("--") {
            return args.next().cloned();
        }
    }
    None
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Split a script into simple commands on `&&`, `||`, `;`, `|`, `&` and newlines, outside
/// quotes. Good enough for the one-liners agents run; anything odd just classifies as nothing.
fn segments(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                } else if c == '\\'
                    && q == '"'
                    && let Some(next) = chars.next()
                {
                    current.push(next);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '\\' => {
                    current.push(c);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ';' | '\n' | '|' => {
                    out.push(std::mem::take(&mut current));
                    if chars.peek() == Some(&'|') {
                        chars.next();
                    }
                }
                '&' => {
                    // `2>&1` and `&>` are redirections, not separators.
                    let redirect = current.ends_with('>') || chars.peek() == Some(&'>');
                    if redirect {
                        current.push(c);
                    } else {
                        out.push(std::mem::take(&mut current));
                        if chars.peek() == Some(&'&') {
                            chars.next();
                        }
                    }
                }
                _ => current.push(c),
            },
        }
    }
    out.push(current);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn words(segment: &str) -> Vec<String> {
    shlex::split(segment)
        .unwrap_or_else(|| segment.split_whitespace().map(str::to_string).collect())
        .into_iter()
        // Redirections are not arguments.
        .filter(|w| !(w.contains('>') || w.starts_with('<')))
        .collect()
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Drop `VAR=x`, `env`, `time`, `nice -n 5`, `timeout 600`, `sudo`, `stdbuf -oL`, ... so the
/// real program comes first.
fn strip_wrappers(mut words: Vec<String>) -> Vec<String> {
    loop {
        let Some(first) = words.first() else {
            return words;
        };
        if is_assignment(first) {
            words.remove(0);
            continue;
        }
        let skip = match basename(first) {
            "env" | "sudo" | "nohup" | "command" | "exec" | "builtin" | "xvfb-run" => {
                1 + words[1..].iter().take_while(|w| w.starts_with('-')).count()
            }
            "time" | "stdbuf" => 1 + words[1..].iter().take_while(|w| w.starts_with('-')).count(),
            "nice" => {
                let mut n = 1;
                while n < words.len() && words[n].starts_with('-') {
                    n += if words[n] == "-n" { 2 } else { 1 };
                }
                n
            }
            "timeout" => {
                let mut n = 1;
                while n < words.len() && words[n].starts_with('-') {
                    n += if matches!(words[n].as_str(), "-s" | "-k" | "--signal" | "--kill-after") {
                        2
                    } else {
                        1
                    };
                }
                n + 1
            }
            _ => 0,
        };
        if skip == 0 {
            return words;
        }
        words.drain(..skip.min(words.len()));
    }
}

fn rank(purpose: Purpose) -> u8 {
    match purpose {
        Purpose::Other => 0,
        Purpose::Configure => 1,
        Purpose::Lint => 2,
        Purpose::Build => 3,
        Purpose::Test => 4,
    }
}

/// The purpose of a whole script: the strongest of its simple commands (`make && ctest` is a
/// test run). `None` when nothing in it configures, builds, tests or lints.
pub(crate) fn classify(script: &str) -> Option<Purpose> {
    segments(script)
        .iter()
        .filter_map(|segment| classify_words(&strip_wrappers(words(segment))))
        .max_by_key(|p| rank(*p))
}

/// The purposes of a script's simple commands, in order (a nested `bash -c` script counts as one).
pub(crate) fn purposes(script: &str) -> Vec<Purpose> {
    segments(script)
        .iter()
        .filter_map(|segment| classify_words(&strip_wrappers(words(segment))))
        .collect()
}

fn non_flags(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter()
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
}

fn classify_words(words: &[String]) -> Option<Purpose> {
    let program = words.first()?;
    let args = &words[1..];
    let name = basename(program).to_ascii_lowercase();
    let name = name.as_str();
    let first_arg = non_flags(args).next().unwrap_or("");

    if is_lint_program(name, args) {
        return Some(Purpose::Lint);
    }
    if args.iter().any(|a| a.starts_with("--gtest_")) {
        return Some(Purpose::Test);
    }
    let purpose = match name {
        "cmake" => {
            if has(args, "--build") {
                if targets_after(args, &["--target", "-t"])
                    .any(|t| matches!(t, "test" | "check" | "RUN_TESTS"))
                {
                    Purpose::Test
                } else {
                    Purpose::Build
                }
            } else if [
                "--install",
                "-E",
                "-P",
                "--version",
                "--help",
                "--system-information",
            ]
            .iter()
            .any(|f| has(args, f))
            {
                return None;
            } else {
                Purpose::Configure
            }
        }
        "ctest" => Purpose::Test,
        "make" | "gmake" | "bmake" | "mingw32-make" | "ninja" | "samu" => {
            if make_targets(args).any(|t| matches!(t, "test" | "check" | "tests" | "unittest")) {
                Purpose::Test
            } else if make_targets(args).any(|t| t == "clean") && make_targets(args).count() == 1 {
                return None;
            } else {
                Purpose::Build
            }
        }
        "meson" | "muon" => match first_arg {
            "setup" | "configure" => Purpose::Configure,
            "compile" => Purpose::Build,
            "test" => Purpose::Test,
            "" => return None,
            _ if args.iter().any(|a| a.starts_with("--")) || !first_arg.contains('.') => {
                // Old syntax: `meson builddir`.
                Purpose::Configure
            }
            _ => return None,
        },
        "configure" | "autogen.sh" | "bootstrap" | "bootstrap.sh" | "autoreconf" | "autoconf" => {
            Purpose::Configure
        }
        "conan" => match first_arg {
            "install" => Purpose::Configure,
            "build" | "create" => Purpose::Build,
            _ => return None,
        },
        "cargo" => match first_arg {
            "build" | "b" | "check" | "c" => Purpose::Build,
            "test" | "t" | "nextest" => Purpose::Test,
            "clippy" => Purpose::Lint,
            _ => return None,
        },
        "go" => match first_arg {
            "build" | "install" => Purpose::Build,
            "test" => Purpose::Test,
            "vet" => Purpose::Lint,
            _ => return None,
        },
        "bazel" | "bazelisk" => match first_arg {
            "build" => Purpose::Build,
            "test" | "coverage" => Purpose::Test,
            _ => return None,
        },
        "pytest" | "py.test" | "nosetests" | "tox" | "nox" | "unittest" => Purpose::Test,
        "python" | "python3" | "python2" => {
            let joined = args.join(" ");
            if joined.starts_with("-m pytest") || joined.starts_with("-m unittest") {
                Purpose::Test
            } else if joined.starts_with("setup.py build") {
                Purpose::Build
            } else {
                return None;
            }
        }
        "npm" | "yarn" | "pnpm" => {
            let script = if first_arg == "run" {
                non_flags(args).nth(1).unwrap_or("")
            } else {
                first_arg
            };
            match script {
                "test" | "t" => Purpose::Test,
                "build" => Purpose::Build,
                "lint" => Purpose::Lint,
                _ => return None,
            }
        }
        "mvn" | "gradle" | "gradlew" | "mvnw" => {
            if non_flags(args).any(|a| a == "test" || a == "check" || a == "verify") {
                Purpose::Test
            } else if non_flags(args)
                .any(|a| matches!(a, "compile" | "package" | "build" | "install" | "assemble"))
            {
                Purpose::Build
            } else {
                return None;
            }
        }
        "scons" | "msbuild" | "b2" | "bjam" | "xcodebuild" => Purpose::Build,
        _ if is_compiler(name) => Purpose::Build,
        _ if SHELLS.contains(&name) => {
            // `bash -c '...'` nests; `bash run_tests.sh` names its script.
            if let Some(script) = shell_script(words) {
                return classify(&script);
            }
            let script = non_flags(args).next()?;
            return classify_script_name(basename(script));
        }
        _ if program.contains('/') => return classify_script_name(name),
        _ => return None,
    };
    Some(purpose)
}

/// A program run by path (`./run_tests.sh`, `build/unit_tests`, `./configure`): its name says
/// what it is for.
fn classify_script_name(name: &str) -> Option<Purpose> {
    let name = name.to_ascii_lowercase();
    if name.contains("test") || name.contains("check") {
        Some(Purpose::Test)
    } else if name.contains("lint") || name.contains("tidy") {
        Some(Purpose::Lint)
    } else if name.contains("configure") || name.contains("bootstrap") || name.contains("autogen") {
        Some(Purpose::Configure)
    } else if name.contains("build") || name.contains("compile") || name == "make.sh" {
        Some(Purpose::Build)
    } else {
        None
    }
}

fn is_compiler(name: &str) -> bool {
    let stem = name.split_once('-').map_or(name, |(stem, version)| {
        if version.chars().all(|c| c.is_ascii_digit() || c == '.') {
            stem
        } else {
            name
        }
    });
    matches!(
        stem,
        "gcc" | "g++" | "cc" | "c++" | "clang" | "clang++" | "nvcc" | "icpx" | "icx"
    )
}

fn is_lint_program(name: &str, args: &[String]) -> bool {
    let tidy = name.starts_with("clang-tidy") || name.starts_with("run-clang-tidy");
    let format_check = name.starts_with("clang-format")
        && args
            .iter()
            .any(|a| matches!(a.as_str(), "--dry-run" | "-n" | "--Werror"));
    tidy || format_check
        || name.starts_with("iwyu")
        || matches!(
            name,
            "cppcheck"
                | "cpplint"
                | "cpplint.py"
                | "include-what-you-use"
                | "clazy-standalone"
                | "scan-build"
                | "flake8"
                | "ruff"
                | "pylint"
                | "mypy"
                | "eslint"
                | "shellcheck"
        )
}

/// Targets named by `--target X` / `-t X` / `--target=X`.
fn targets_after<'a>(args: &'a [String], flags: &'a [&'a str]) -> impl Iterator<Item = &'a str> {
    args.iter().enumerate().filter_map(move |(i, a)| {
        if flags.contains(&a.as_str()) {
            args.get(i + 1).map(String::as_str)
        } else {
            flags
                .iter()
                .find_map(|f| a.strip_prefix(&format!("{f}=")))
                .map(|_| a.split_once('=').map_or("", |(_, v)| v))
        }
    })
}

/// make/ninja targets: non-flag words that are not the value of `-C`, `-f`, `-j`, `-l`, `-k`.
fn make_targets(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter().enumerate().filter_map(|(i, a)| {
        if a.starts_with('-') || is_assignment(a) {
            return None;
        }
        let previous = i
            .checked_sub(1)
            .and_then(|p| args.get(p))
            .map(String::as_str);
        if matches!(
            previous,
            Some("-C" | "-f" | "-j" | "-l" | "-k" | "-d" | "-t" | "-w")
        ) {
            return None;
        }
        Some(a.as_str())
    })
}

/// The JUnit XML files a script asks its test runner to write, relative to `cwd` (or
/// absolute). Follows `cd DIR` between commands.
pub(crate) fn junit_outputs(script: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut dir = cwd.to_path_buf();
    let mut out = Vec::new();
    for segment in segments(script) {
        let words = strip_wrappers(words(&segment));
        let Some(program) = words.first() else {
            continue;
        };
        let args = &words[1..];
        let name = basename(program);
        if matches!(name, "cd" | "pushd") {
            if let Some(target) = args.first().filter(|t| *t != "-") {
                dir = dir.join(target);
            }
            continue;
        }
        if name == "ctest" {
            if let Some(file) = flag_value(args, "--output-junit") {
                let test_dir = match flag_value(args, "--test-dir") {
                    Some(test_dir) => dir.join(test_dir),
                    None => dir.clone(),
                };
                out.push(test_dir.join(file));
            }
        } else if matches!(name, "meson" | "muon") && non_flags(args).next() == Some("test") {
            let build_dir = match flag_value(args, "-C") {
                Some(build_dir) => dir.join(build_dir),
                None => dir.clone(),
            };
            out.push(build_dir.join("meson-logs/testlog.junit.xml"));
        } else if let Some(file) =
            flag_value(args, "--junitxml").or_else(|| flag_value(args, "--junit-xml"))
        {
            out.push(dir.join(file));
        } else if let Some(spec) = flag_value(args, "--gtest_output") {
            let spec = spec.strip_prefix("xml").map(|s| s.trim_start_matches(':'));
            match spec {
                Some("") => out.push(dir.join("test_detail.xml")),
                Some(path) if path.ends_with('/') => {
                    out.push(dir.join(path).join(format!("{name}.xml")));
                }
                Some(path) => out.push(dir.join(path)),
                None => {}
            }
        } else if (has_value(args, "-r", "junit") || has_value(args, "--reporter", "junit"))
            && let Some(file) = flag_value(args, "-o").or_else(|| flag_value(args, "--out"))
        {
            out.push(dir.join(file));
        }
    }
    out
}

/// `--flag VALUE` or `--flag=VALUE`.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(i, a)| {
        if a == flag {
            args.get(i + 1).cloned()
        } else {
            a.strip_prefix(flag)
                .and_then(|rest| rest.strip_prefix('='))
                .map(str::to_string)
        }
    })
}

fn has_value(args: &[String], flag: &str, value: &str) -> bool {
    flag_value(args, flag).as_deref() == Some(value)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn unwraps_shell_wrappers() {
        assert_eq!(
            unwrap_shell("/bin/bash -lc 'cmake --build build -j4'"),
            "cmake --build build -j4"
        );
        assert_eq!(unwrap_shell("bash -l -c 'make test'"), "make test");
        assert_eq!(
            unwrap_shell("ctest --test-dir build"),
            "ctest --test-dir build"
        );
        assert_eq!(unwrap_shell("bash run_tests.sh"), "bash run_tests.sh");
    }

    #[test]
    fn classifies_common_commands() {
        let cases: &[(&str, Option<Purpose>)] = &[
            ("cmake -S . -B build -G Ninja", Some(Purpose::Configure)),
            ("cmake --preset ci", Some(Purpose::Configure)),
            ("cmake --build build -j4", Some(Purpose::Build)),
            ("cmake --build build --target test", Some(Purpose::Test)),
            ("cmake -E make_directory build", None),
            ("./configure --prefix=/usr", Some(Purpose::Configure)),
            ("meson setup build", Some(Purpose::Configure)),
            ("meson compile -C build", Some(Purpose::Build)),
            ("meson test -C build", Some(Purpose::Test)),
            ("make -j8", Some(Purpose::Build)),
            ("make -C build test", Some(Purpose::Test)),
            ("ninja -C build", Some(Purpose::Build)),
            ("ninja -C build check", Some(Purpose::Test)),
            ("cd build && make -j8 2>&1 | tail -20", Some(Purpose::Build)),
            (
                "cmake --build build && ctest --test-dir build --output-on-failure",
                Some(Purpose::Test),
            ),
            ("ctest --test-dir build -R Book", Some(Purpose::Test)),
            (
                "CTEST_OUTPUT_ON_FAILURE=1 timeout 600 ctest",
                Some(Purpose::Test),
            ),
            (
                "./build/tests/book_test --gtest_filter=Book.*",
                Some(Purpose::Test),
            ),
            ("build/unit_tests", Some(Purpose::Test)),
            ("./run_tests.sh", Some(Purpose::Test)),
            ("./build.sh", Some(Purpose::Build)),
            ("bash scripts/run_tests.sh", Some(Purpose::Test)),
            ("python3 -m pytest -q tests", Some(Purpose::Test)),
            ("cargo test -p foo", Some(Purpose::Test)),
            ("g++-13 -O2 -c book.cpp -o book.o", Some(Purpose::Build)),
            ("clang-tidy -p build src/book.cpp", Some(Purpose::Lint)),
            ("run-clang-tidy-18 -p build", Some(Purpose::Lint)),
            ("cppcheck --enable=all src", Some(Purpose::Lint)),
            ("clang-format -i src/book.cpp", None),
            (
                "clang-format --dry-run --Werror src/book.cpp",
                Some(Purpose::Lint),
            ),
            ("ls -la && cat CMakeLists.txt", None),
            ("grep -rn 'ctest' docs", None),
            ("git diff", None),
            ("make clean", None),
            ("echo 'make test; ctest'", None),
        ];
        for (script, expected) in cases {
            assert_eq!(classify(script), *expected, "{script}");
        }
    }

    #[test]
    fn finds_junit_outputs() {
        let cwd = Path::new("/r");
        assert_eq!(
            junit_outputs("ctest --test-dir build --output-junit junit.xml", cwd),
            vec![cwd.join("build/junit.xml")]
        );
        assert_eq!(
            junit_outputs("cd build && ctest --output-junit=out/j.xml", cwd),
            vec![cwd.join("build/out/j.xml")]
        );
        assert_eq!(
            junit_outputs("./build/book_test --gtest_output=xml:reports/", cwd),
            vec![cwd.join("reports/").join("book_test.xml")]
        );
        assert_eq!(
            junit_outputs("pytest --junitxml=/tmp/p.xml", cwd),
            vec![PathBuf::from("/tmp/p.xml")]
        );
        assert_eq!(
            junit_outputs("ctest --test-dir build", cwd),
            Vec::<PathBuf>::new()
        );
    }
}
