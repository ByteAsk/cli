//! Command families.
//!
//! Command sniffing cannot prove that an output is NOT a file read
//! (`bash -lc "cd x && cat y | head"`, scripts, aliases). So the rule is inverted: the
//! default is [`Family::Unknown`], which gets only the two terminal-equivalent
//! transforms, and every other transform needs a POSITIVE match on a family whose output
//! cannot be source text the model will patch by line.
//!
//! A command is matched only when it is simple: an optional `bash -lc` / `sh -c` wrapper,
//! optional `cd DIR &&`, optional `VAR=value` assignments, ONE program, and at most a
//! trailing `2>&1` and a trailing `| head …` / `| tail …`. Anything else is `Unknown`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    Unknown,
    Search,
    Listing,
    Diff,
    Build,
    Test,
    Install,
    Log,
    Network,
}

impl Family {
    /// Key of this family's budget in the policy object.
    pub fn budget_key(self) -> &'static str {
        match self {
            Family::Unknown => "other",
            Family::Search => "search",
            Family::Listing => "listing",
            Family::Diff => "diff",
            Family::Build | Family::Test | Family::Install | Family::Log => "build_test",
            Family::Network => "other",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Family::Unknown => "unknown",
            Family::Search => "search",
            Family::Listing => "listing",
            Family::Diff => "diff",
            Family::Build => "build",
            Family::Test => "test",
            Family::Install => "install",
            Family::Log => "log",
            Family::Network => "network",
        }
    }
}

/// Classify a command line. `Unknown` whenever there is any doubt.
pub fn classify(command: &str) -> Family {
    let Some(words) = simple_words(command) else {
        return Family::Unknown;
    };
    let Some(first) = words.first() else {
        return Family::Unknown;
    };
    let prog = first.rsplit('/').next().unwrap_or(first);
    let sub = words.get(1).map(String::as_str).unwrap_or("");
    let has = |flag: &str| words.iter().skip(1).any(|w| w == flag);
    let has_short = |c: char| {
        words
            .iter()
            .skip(1)
            .any(|w| w.starts_with('-') && !w.starts_with("--") && w.contains(c))
    };
    match prog {
        "rg" | "ag" => {
            // `rg --files` and `rg -l` are listings; `rg --json` is not line-oriented.
            if has("--files") || has("--json") || has_short('l') {
                Family::Unknown
            } else {
                Family::Search
            }
        }
        "grep" | "egrep" | "fgrep" => {
            if (has_short('r') || has_short('R')) && has_short('n') && !has_short('l') {
                Family::Search
            } else {
                Family::Unknown
            }
        }
        "git" => match sub {
            "grep" if has_short('n') => Family::Search,
            "diff" | "show" => Family::Diff,
            "clone" | "fetch" | "pull" => Family::Install,
            _ => Family::Unknown,
        },
        "diff" => Family::Diff,
        "find" | "fd" | "fdfind" | "tree" => {
            // `find … -exec cat {}` prints file content.
            if has("-exec") || has("-execdir") || has("-x") || has("--exec") {
                Family::Unknown
            } else {
                Family::Listing
            }
        }
        "ls" if has_short('R') => Family::Listing,
        "make" | "gmake" | "ninja" | "cmake" | "bazel" | "mvn" | "gradle" | "gradlew" | "tsc"
        | "gcc" | "g++" | "cc" | "c++" | "clang" | "clang++" | "msbuild" | "xcodebuild" => {
            if matches!(sub, "test" | "check") && matches!(prog, "make" | "gmake" | "bazel") {
                Family::Test
            } else {
                Family::Build
            }
        }
        "cargo" => match sub {
            "build" | "check" | "clippy" | "doc" | "b" | "c" => Family::Build,
            "test" | "nextest" | "t" => Family::Test,
            "install" | "fetch" | "update" | "add" => Family::Install,
            _ => Family::Unknown,
        },
        "go" => match sub {
            "build" | "vet" => Family::Build,
            "test" => Family::Test,
            "get" | "mod" | "install" => Family::Install,
            _ => Family::Unknown,
        },
        "pytest" | "py.test" | "jest" | "vitest" | "ctest" | "tox" | "nosetests" | "phpunit"
        | "rspec" => Family::Test,
        "python" | "python3" => {
            if sub == "-m" {
                match words.get(2).map(String::as_str).unwrap_or("") {
                    "pytest" | "unittest" => Family::Test,
                    "pip" => match words.get(3).map(String::as_str).unwrap_or("") {
                        "install" | "download" | "wheel" => Family::Install,
                        _ => Family::Unknown,
                    },
                    _ => Family::Unknown,
                }
            } else {
                Family::Unknown
            }
        }
        "pip" | "pip3" | "uv" | "poetry" | "conda" | "mamba" => match sub {
            "install" | "download" | "wheel" | "sync" | "add" | "update" | "create" => {
                Family::Install
            }
            "pip" if words.get(2).is_some_and(|w| w == "install") => Family::Install,
            _ => Family::Unknown,
        },
        "npm" | "pnpm" | "yarn" | "bun" => match sub {
            "install" | "i" | "ci" | "add" | "update" | "upgrade" => Family::Install,
            "test" | "t" => Family::Test,
            "run" => match words.get(2).map(String::as_str).unwrap_or("") {
                "build" | "compile" | "typecheck" | "lint" => Family::Build,
                "test" => Family::Test,
                _ => Family::Unknown,
            },
            "" if prog == "yarn" => Family::Install,
            _ => Family::Unknown,
        },
        "apt" | "apt-get" | "dnf" | "yum" | "apk" | "pacman" | "brew" | "zypper" => match sub {
            "install" | "update" | "upgrade" | "add" | "-S" | "-Syu" | "dist-upgrade" => {
                Family::Install
            }
            _ => Family::Unknown,
        },
        "docker" | "podman" => match sub {
            "pull" | "build" | "push" => Family::Install,
            "logs" => Family::Log,
            _ => Family::Unknown,
        },
        "journalctl" | "dmesg" => Family::Log,
        "kubectl" => match sub {
            "logs" => Family::Log,
            "get" | "describe"
                if words
                    .iter()
                    .any(|w| w == "json" || w == "-ojson" || w == "-o=json") =>
            {
                Family::Network
            }
            _ => Family::Unknown,
        },
        "curl" | "http" | "https" | "xh" => {
            // A download to a file prints progress, not a body.
            Family::Network
        }
        "gh" if sub == "api" => Family::Network,
        "aws" | "gcloud" | "az" => Family::Network,
        _ => Family::Unknown,
    }
}

/// Split a SIMPLE command into words, or `None` when it is not simple.
fn simple_words(command: &str) -> Option<Vec<String>> {
    let mut cmd = command.trim();
    // Unwrap `bash -lc '…'`, `sh -c "…"`, `zsh -lc …`.
    for shell in [
        "bash",
        "sh",
        "zsh",
        "/bin/bash",
        "/bin/sh",
        "/bin/zsh",
        "/usr/bin/bash",
    ] {
        for flag in ["-lc", "-c", "-ic"] {
            let prefix = format!("{shell} {flag} ");
            if let Some(rest) = cmd.strip_prefix(&prefix) {
                let rest = rest.trim();
                cmd = unquote_whole(rest)?;
            }
        }
    }
    let mut cmd = cmd.trim().to_string();
    // Optional `cd DIR &&`.
    if cmd.starts_with("cd ") {
        let idx = cmd.find("&&")?;
        let dir = cmd[3..idx].trim();
        if dir.is_empty() || dir.contains(['|', ';', '$', '`', '>', '<']) {
            return None;
        }
        cmd = cmd[idx + 2..].trim().to_string();
    }
    // Optional trailing `| head …` / `| tail …`.
    if let Some(idx) = cmd.rfind('|') {
        let tail = cmd[idx + 1..].trim();
        let first = tail.split_whitespace().next().unwrap_or("");
        if !matches!(first, "head" | "tail") || tail.contains(['|', ';', '&', '$', '`', '>', '<']) {
            return None;
        }
        // `||` is not a pipe.
        if idx > 0 && cmd.as_bytes()[idx - 1] == b'|' {
            return None;
        }
        cmd = cmd[..idx].trim().to_string();
    }
    // Optional trailing `2>&1`.
    if let Some(rest) = cmd.strip_suffix("2>&1") {
        cmd = rest.trim().to_string();
    }
    if cmd.contains(['|', ';', '&', '$', '`', '>', '<', '\n', '(', ')']) {
        return None;
    }
    let mut words = split_words(&cmd)?;
    // Optional leading `VAR=value` assignments and `env`.
    while let Some(first) = words.first() {
        let is_assignment = first.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if is_assignment || first == "env" || first == "time" || first == "nice" {
            words.remove(0);
        } else {
            break;
        }
    }
    if words.is_empty() { None } else { Some(words) }
}

/// `'…'` or `"…"` spanning the WHOLE string, with no escapes inside; else `None`.
fn unquote_whole(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let q = bytes[0];
    if (q != b'\'' && q != b'"') || bytes[bytes.len() - 1] != q {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    if inner.contains(q as char) || inner.contains('\\') {
        return None;
    }
    Some(inner)
}

/// Whitespace split that understands plain quotes. `None` on an unbalanced quote.
fn split_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn plain_commands_are_matched() {
        assert_eq!(classify("rg -n foo src"), Family::Search);
        assert_eq!(classify("grep -rn foo ."), Family::Search);
        assert_eq!(classify("git grep -n foo"), Family::Search);
        assert_eq!(classify("git diff HEAD~1"), Family::Diff);
        assert_eq!(classify("find . -name '*.rs'"), Family::Listing);
        assert_eq!(classify("cargo build --release"), Family::Build);
        assert_eq!(classify("cargo test -p core"), Family::Test);
        assert_eq!(classify("python3 -m pytest -q"), Family::Test);
        assert_eq!(classify("pip install numpy"), Family::Install);
        assert_eq!(classify("npm ci"), Family::Install);
        assert_eq!(classify("journalctl -u x"), Family::Log);
        assert_eq!(classify("curl -s https://x/y"), Family::Network);
    }

    #[test]
    fn wrappers_cd_env_and_a_trailing_tail_are_seen_through() {
        assert_eq!(
            classify("bash -lc 'cargo build 2>&1 | tail -40'"),
            Family::Build
        );
        assert_eq!(classify("bash -lc \"cd sub && make -j4\""), Family::Build);
        assert_eq!(classify("RUSTFLAGS=-Dwarnings cargo check"), Family::Build);
        assert_eq!(classify("/usr/bin/rg -n x"), Family::Search);
    }

    #[test]
    fn anything_that_could_print_a_file_is_unknown() {
        for cmd in [
            "cat src/main.rs",
            "sed -n '1,40p' a.py",
            "head -50 x.c",
            "bash -lc 'cd x && cat y | head'",
            "rg -n foo | xargs cat",
            "cargo build && cat target/out.txt",
            "make; cat log",
            "find . -name '*.c' -exec cat {} +",
            "python3 script.py",
            "echo $(cat secret)",
            "rg --files",
            "rg -l foo",
            "grep foo file.txt",
            "ls -la",
            "git log",
            "jq . file.json",
            "",
        ] {
            assert_eq!(classify(cmd), Family::Unknown, "{cmd}");
        }
    }

    #[test]
    fn an_unbalanced_quote_is_unknown() {
        assert_eq!(classify("rg -n 'foo"), Family::Unknown);
    }
}
