//! Program arguments the model passes as ONE string: `run_args` on
//! `cpp_sanitize`, `args` on `debug`, `debug_session` and `debug_record`.
//!
//! The string is read the way a POSIX shell reads a command line — words split
//! on whitespace, `'…'` and `"…"` quoting, backslash escapes, `""` is an empty
//! argument — but NO shell ever runs. Splitting on whitespace alone (what all
//! four tools did before) made an empty argument, or one containing a space,
//! impossible to express, and passed quote characters through literally: a
//! user asked for an empty-string key, the model sent `run_args: "\"\""`, the
//! program received the two-character string `""`, and the model then reported
//! a result for the empty string that it had never run.
//!
//! Shell syntax that only a shell could act on (pipes, redirection, `;`, `&`,
//! command substitution, `$VAR`, a leading `~`) is REFUSED rather than passed
//! through as literal words. Models do send it — production calls include
//! `" 2>&1 | head -100"` and `"> /tmp/debug_sema.log 2>&1; exit"` — and under
//! the old splitter each of those reached the program as extra arguments while
//! the verdict still read "clean". An error the model can act on beats a run of
//! a different command line. Quoting the character passes it literally.

/// Parse `input` into the program's argument list. `field` names the tool
/// argument (`run_args`, `args`) for the error message.
pub(crate) fn parse(field: &str, input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    // A word exists once anything — even an empty quote pair — has started it.
    let mut in_word = false;
    let mut chars = input.char_indices().peekable();

    while let Some((at, c)) = chars.next() {
        match c {
            '\0' => {
                return Err(format!(
                    "{field} contains a NUL byte, which no program argument can hold."
                ));
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some((_, '\'')) => break,
                        Some((_, '\0')) => {
                            return Err(format!(
                                "{field} contains a NUL byte, which no program argument can hold."
                            ));
                        }
                        Some((_, other)) => word.push(other),
                        None => return Err(unterminated(field, '\'')),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some((_, '"')) => break,
                        // Inside double quotes a backslash escapes only these
                        // (POSIX); before anything else it is itself literal.
                        Some((_, '\\')) => match chars.peek().map(|&(_, next)| next) {
                            Some('\n') => {
                                chars.next();
                            }
                            Some(next @ ('"' | '\\' | '$' | '`')) => {
                                chars.next();
                                word.push(next);
                            }
                            _ => word.push('\\'),
                        },
                        // A shell expands these inside double quotes too.
                        Some((inner, '$')) if starts_expansion(chars.peek().map(|&(_, n)| n)) => {
                            return Err(shell_only(field, input, inner, '$'));
                        }
                        Some((inner, '`')) => return Err(shell_only(field, input, inner, '`')),
                        Some((_, '\0')) => {
                            return Err(format!(
                                "{field} contains a NUL byte, which no program argument can hold."
                            ));
                        }
                        Some((_, other)) => word.push(other),
                        None => return Err(unterminated(field, '"')),
                    }
                }
            }
            '\\' => match chars.next() {
                // Line continuation: the pair disappears.
                Some((_, '\n')) => {}
                Some((_, '\0')) => {
                    return Err(format!(
                        "{field} contains a NUL byte, which no program argument can hold."
                    ));
                }
                Some((_, next)) => {
                    in_word = true;
                    word.push(next);
                }
                None => {
                    return Err(format!(
                        "{field} ends with a lone backslash. Escape it as \\\\ or quote it: '\\'."
                    ));
                }
            },
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' => {
                return Err(shell_only(field, input, at, c));
            }
            '$' if starts_expansion(chars.peek().map(|&(_, next)| next)) => {
                return Err(shell_only(field, input, at, c));
            }
            '~' if !in_word => return Err(shell_only(field, input, at, c)),
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Quote one argument for a POSIX `sh`: single quotes make every character
/// literal, and an embedded `'` becomes `'\''`. Used where an argument list
/// must survive a shell that we do not control the parsing of (gdb's
/// `startup-with-shell`).
pub(crate) fn sh_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Would a shell treat `$` followed by `next` as an expansion?
fn starts_expansion(next: Option<char>) -> bool {
    matches!(next, Some(c) if c.is_ascii_alphanumeric()
        || matches!(c, '_' | '{' | '(' | '@' | '*' | '#' | '?' | '$' | '!' | '-'))
}

fn unterminated(field: &str, quote: char) -> String {
    format!(
        "{field} has an unterminated {quote} quote. Close it, or escape a literal quote as \\{quote}."
    )
}

fn shell_only(field: &str, input: &str, at: usize, c: char) -> String {
    let start = input[..at]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(i, ch)| i + ch.len_utf8());
    let word = input[start..].split_whitespace().next().unwrap_or("");
    format!(
        "{field} is not run through a shell: it is split into the program's argument list and \
         nothing else, so `{c}` (in `{word}`) would reach the program as a literal argument. \
         Pipes, redirection, `;`, `&`, $VAR, command substitution and `~` are not available; the \
         program's stdout and stderr are already captured and returned to you. To pass the \
         character itself as an argument, quote it: '{c}'."
    )
}

/// A real program that records the argument list it was started with, so the
/// transport tests measure what arrived rather than what we meant to send.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;
    use std::path::PathBuf;

    /// One run as the program saw it.
    #[derive(Debug, PartialEq)]
    pub(crate) struct Run {
        pub args: Vec<String>,
        /// `$SHELL` in the program's environment, `None` when unset.
        pub shell: Option<String>,
    }

    /// Compile the recorder into `dir`. It appends each run to `dir/argv.out`
    /// (an absolute path compiled in, so the program's cwd does not matter).
    /// `None` when there is no C compiler — the caller skips.
    pub(crate) fn build(dir: &Path) -> Option<PathBuf> {
        let source = dir.join("argv_recorder.c");
        std::fs::write(
            &source,
            "#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n\
             int main(int argc, char **argv) {\n\
               FILE *f = fopen(OUT, \"a\");\n\
               const char *sh = getenv(\"SHELL\");\n\
               fprintf(f, \"RUN %d %s\\n\", argc - 1, sh ? sh : \"\");\n\
               for (int i = 1; i < argc; i++) {\n\
                 fprintf(f, \"%zu:\", strlen(argv[i]));\n\
                 fwrite(argv[i], 1, strlen(argv[i]), f);\n\
                 fputc('\\n', f);\n\
               }\n\
               fclose(f);\n\
               return 0;\n\
             }\n",
        )
        .ok()?;
        let out = dir.join("argv.out");
        let binary = dir.join("argv_recorder");
        let status = std::process::Command::new("cc")
            .arg(format!("-DOUT=\"{}\"", out.display()))
            .arg("-g")
            .arg("-o")
            .arg(&binary)
            .arg(&source)
            .status()
            .ok()?;
        status.success().then_some(binary)
    }

    /// Every run recorded so far, oldest first.
    pub(crate) fn runs(dir: &Path) -> Vec<Run> {
        let Ok(bytes) = std::fs::read(dir.join("argv.out")) else {
            return Vec::new();
        };
        let text = String::from_utf8(bytes).expect("utf-8 record");
        let mut rest = text.as_str();
        let mut runs = Vec::new();
        while let Some(line_end) = rest.find('\n') {
            let header = &rest["RUN ".len()..line_end];
            let (count, shell) = header.split_once(' ').expect("header");
            rest = &rest[line_end + 1..];
            let mut args = Vec::new();
            for _ in 0..count.parse::<usize>().expect("count") {
                let (len, tail) = rest.split_once(':').expect("length prefix");
                let len: usize = len.parse().expect("length");
                args.push(tail[..len].to_string());
                rest = &tail[len + 1..];
            }
            runs.push(Run {
                args,
                shell: (!shell.is_empty()).then(|| shell.to_string()),
            });
        }
        runs
    }

    pub(crate) fn have(tool: &str) -> bool {
        std::process::Command::new(tool)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    /// Argument lists that exercise both gdb transports: everything in the
    /// first is passed raw, anything in the second needs the quoted shell.
    pub(crate) fn hard_cases() -> Vec<Vec<String>> {
        let owned = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        vec![
            owned(&[
                "-v",
                "x\"y",
                "back\\slash",
                "$HOME",
                "*",
                "it's",
                "日本",
                "--",
                "-",
            ]),
            owned(&["", "a b", " lead", "trail ", "it's", "\t", "", "2>&1 | x"]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ok(input: &str) -> Vec<String> {
        parse("run_args", input).unwrap_or_else(|err| panic!("{input:?}: {err}"))
    }

    fn err(input: &str) -> String {
        parse("run_args", input).expect_err(input)
    }

    #[test]
    fn empty_quotes_are_an_empty_argument() {
        // The reported case: `""` must be ONE empty argument, not two quote chars.
        assert_eq!(ok(r#""""#), vec![""]);
        assert_eq!(ok("''"), vec![""]);
        assert_eq!(ok(r#"a "" b"#), vec!["a", "", "b"]);
        assert_eq!(ok(r#""" """#), vec!["", ""]);
    }

    #[test]
    fn nothing_is_no_arguments() {
        assert_eq!(ok(""), Vec::<String>::new());
        assert_eq!(ok(" \t\n "), Vec::<String>::new());
    }

    #[test]
    fn plain_words_split_on_any_whitespace() {
        assert_eq!(
            ok("--capacity 4  data/trace_evict.txt"),
            vec!["--capacity", "4", "data/trace_evict.txt"]
        );
        assert_eq!(ok("a\tb\nc"), vec!["a", "b", "c"]);
        assert_eq!(ok(" 6000 "), vec!["6000"]);
    }

    #[test]
    fn quotes_keep_whitespace_and_join_adjacent_pieces() {
        assert_eq!(ok(r#""a b" c"#), vec!["a b", "c"]);
        assert_eq!(ok("'a  b'"), vec!["a  b"]);
        assert_eq!(ok(r#"--name="John Smith""#), vec!["--name=John Smith"]);
        assert_eq!(ok(r#"x"y z"'w'"#), vec!["xy zw"]);
    }

    #[test]
    fn single_quotes_are_fully_literal() {
        assert_eq!(ok(r"'a\b'"), vec![r"a\b"]);
        assert_eq!(ok("'$HOME'"), vec!["$HOME"]);
        assert_eq!(ok("'2>&1 | x'"), vec!["2>&1 | x"]);
        assert_eq!(ok(r#"'say "hi"'"#), vec![r#"say "hi""#]);
    }

    #[test]
    fn double_quote_backslash_escapes_only_the_posix_set() {
        assert_eq!(ok(r#""a\"b""#), vec![r#"a"b"#]);
        assert_eq!(ok(r#""a\\b""#), vec![r"a\b"]);
        assert_eq!(ok(r#""\$HOME""#), vec!["$HOME"]);
        // Before any other character the backslash stays.
        assert_eq!(ok(r#""a\nb""#), vec![r"a\nb"]);
        assert_eq!(ok(r#""C:\dir""#), vec![r"C:\dir"]);
    }

    #[test]
    fn unquoted_backslash_escapes_the_next_character() {
        assert_eq!(ok(r"a\ b"), vec!["a b"]);
        assert_eq!(ok(r#"\"\""#), vec![r#""""#]);
        assert_eq!(ok(r"\'"), vec!["'"]);
        assert_eq!(ok(r"a\\b"), vec![r"a\b"]);
        assert_eq!(ok("a\\\nb"), vec!["ab"]);
        assert_eq!(ok(r"\|"), vec!["|"]);
    }

    #[test]
    fn a_lone_dollar_and_globs_are_literal() {
        // A shell passes these through unchanged too (a glob that matches
        // nothing stays literal), and filters like `BM_.*` depend on it.
        assert_eq!(ok("$ 5$"), vec!["$", "5$"]);
        assert_eq!(
            ok("BM_.* a?b [x] {a,b} #1"),
            vec!["BM_.*", "a?b", "[x]", "{a,b}", "#1"]
        );
        assert_eq!(ok("a~b"), vec!["a~b"]);
        assert_eq!(ok("--x=-1 -- -"), vec!["--x=-1", "--", "-"]);
    }

    #[test]
    fn unicode_is_preserved_byte_for_byte() {
        assert_eq!(ok("héllo '日本 語' 🚀"), vec!["héllo", "日本 語", "🚀"]);
        // A non-breaking space is whitespace to Rust; quoting keeps it.
        assert_eq!(ok("'a\u{a0}b'"), vec!["a\u{a0}b"]);
    }

    #[test]
    fn shell_operators_are_refused_not_passed_through() {
        // Every one of these was seen in real use or is its obvious sibling.
        for input in [
            " 2>&1 | head -100",
            "> /tmp/debug_sema.log 2>&1; exit",
            "test_nested 2>&1",
            "< input.txt",
            "a;b",
            "a && b",
            "a || b",
            "sleep 1 &",
            "$(whoami)",
            "`whoami`",
            "(a)",
        ] {
            let message = err(input);
            assert!(
                message.contains("not run through a shell"),
                "{input}: {message}"
            );
            assert!(message.contains("run_args"), "{input}: {message}");
        }
    }

    #[test]
    fn expansions_are_refused_even_inside_double_quotes() {
        for input in [
            "$HOME/x",
            "${HOME}",
            "$1",
            "$@",
            r#""$HOME""#,
            r#""`id`""#,
            "~/data.txt",
            "~",
        ] {
            assert!(err(input).contains("not run through a shell"), "{input}");
        }
    }

    #[test]
    fn the_error_names_the_offending_word() {
        let message = err("data.txt 2>&1");
        assert!(message.contains("`>` (in `2>&1`)"), "{message}");
    }

    #[test]
    fn quoting_passes_every_refused_character_literally() {
        assert_eq!(
            ok(r#"'|' '&' ';' '<' '>' '(' ')' '`' '$HOME' '~' "~/x""#),
            vec!["|", "&", ";", "<", ">", "(", ")", "`", "$HOME", "~", "~/x"]
        );
    }

    #[test]
    fn malformed_input_is_an_error_not_a_guess() {
        assert!(err(r#""abc"#).contains("unterminated \" quote"));
        assert!(err("'abc").contains("unterminated ' quote"));
        assert!(err("abc\\").contains("lone backslash"));
        assert!(err("a\0b").contains("NUL"));
        assert!(err("'a\0b'").contains("NUL"));
        assert!(err("a\\\0b").contains("NUL"));
    }

    #[test]
    fn sh_quote_round_trips_through_the_parser() {
        for arg in [
            "",
            "a b",
            "it's",
            r#"x"y"#,
            r"back\slash",
            "$HOME",
            "'",
            "''",
            "日本",
            "-v",
            "a\tb",
        ] {
            assert_eq!(ok(&sh_quote(arg)), vec![arg.to_string()], "{arg:?}");
        }
    }
}
