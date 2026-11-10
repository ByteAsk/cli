//! What changed: files, the entities (functions, types, macros) the change
//! touches, string literals on changed lines, and an index of the test cases
//! in a test source file.
//!
//! Symbols come from two places in `git diff -U0 <base>` with git's built-in
//! `cpp` userdiff driver: the enclosing function git names in each hunk header,
//! and declarations on the changed lines themselves (the hunk header of a hunk
//! that edits a signature names the PREVIOUS function). Both are textual: good
//! for ranking and for finding tests that mention an entity, not a proof.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex_lite::Regex;

use super::ident_char;
use super::is_build_file;
use super::is_cpp_file;
use crate::gitx;

const TOOL: &str = "cpp_test_select";
/// Most symbols kept (the rest are named in a count).
pub(crate) const SYMBOL_CAP: usize = 16;
const LITERAL_CAP: usize = 8;
const UNTRACKED_CAP: usize = 200;

/// The change being tested.
#[derive(Clone, Debug, Default)]
pub(crate) struct Changes {
    /// Absolute paths of changed tracked files and new untracked source/build files.
    pub files: Vec<PathBuf>,
    /// Changed entity names, most relevant first.
    pub symbols: Vec<String>,
    /// How many more were found than kept.
    pub symbols_dropped: usize,
    /// String literals on changed lines (diagnostics, keys), for neighbour search.
    pub literals: Vec<String>,
    /// New-side line ranges per changed file (whole file for new files).
    pub lines: BTreeMap<PathBuf, Vec<(u32, u32)>>,
    /// What it was compared with, for the output ("session-start HEAD abc123").
    pub base_label: String,
}

impl Changes {
    pub(crate) fn cpp_files(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter().filter(|f| is_cpp_file(f))
    }

    pub(crate) fn touches_build_files(&self) -> Vec<&PathBuf> {
        self.files.iter().filter(|f| is_build_file(f)).collect()
    }
}

/// Collect the change in the work tree at `root` against revision `base`.
/// `files`/`symbols` override what the diff says (relative to `root`).
pub(crate) async fn changes(
    root: &Path,
    base: &str,
    base_label: String,
    files: Option<&[String]>,
    symbols: Option<&[String]>,
) -> Result<Changes, String> {
    let mut out = Changes {
        base_label,
        ..Changes::default()
    };
    let tracked = gitx::git_str(
        TOOL,
        None,
        root,
        &["diff", "--name-only", "--no-renames", "-z", base, "--"],
        gitx::GIT_TIMEOUT,
    )
    .await;
    if !tracked.ok {
        return Err(format!(
            "git diff against {base} failed: {}",
            tracked.stderr.trim()
        ));
    }
    let mut paths: Vec<PathBuf> = tracked
        .stdout
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(|p| root.join(p))
        .collect();
    let untracked = gitx::git_str(
        TOOL,
        None,
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        gitx::GIT_TIMEOUT,
    )
    .await;
    let mut new_files = Vec::new();
    if untracked.ok {
        for rel in untracked.stdout.split('\0').filter(|p| !p.is_empty()) {
            let path = root.join(rel);
            let first = Path::new(rel).components().next().map(|c| root.join(c));
            if first.is_some_and(|dir| dir.is_dir() && super::skip_dir(&dir)) {
                continue;
            }
            if (is_cpp_file(&path) || is_build_file(&path)) && new_files.len() < UNTRACKED_CAP {
                new_files.push(path);
            }
        }
    }
    if let Some(explicit) = files {
        paths = explicit
            .iter()
            .map(|f| super::normalize(root, Path::new(f)))
            .collect();
        new_files.retain(|f| paths.contains(f));
    } else {
        paths.extend(new_files.iter().cloned());
    }
    paths.sort();
    paths.dedup();
    out.files = paths;

    // Symbols and literals from the diff text of tracked C/C++ files.
    let cpp_rel: Vec<String> = out
        .files
        .iter()
        .filter(|f| is_cpp_file(f) && !new_files.contains(f))
        .filter_map(|f| {
            f.strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .collect();
    let mut found = Found::default();
    if !cpp_rel.is_empty() {
        let attributes = attributes_file();
        let mut args = vec![
            "-c".to_string(),
            format!("core.attributesFile={}", attributes.display()),
            "diff".to_string(),
            "-U0".to_string(),
            "--no-color".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            base.to_string(),
            "--".to_string(),
        ];
        args.extend(cpp_rel);
        let diff = gitx::git_logged(TOOL, None, root, &args, gitx::GIT_TIMEOUT).await;
        if diff.ok {
            parse_diff(&diff.stdout, root, &mut found);
        }
    }
    for file in &new_files {
        if !is_cpp_file(file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let count = u32::try_from(text.lines().count())
            .unwrap_or(u32::MAX)
            .max(1);
        found
            .lines
            .entry(file.clone())
            .or_default()
            .push((1, count));
        for line in text.lines() {
            found.declared(line);
            found.literals(line);
        }
    }
    let ranked = found.ranked();
    out.lines = found.lines;
    out.literals = found.literal_list;
    match symbols {
        Some(explicit) => out.symbols = explicit.iter().map(|s| last_component(s)).collect(),
        None => {
            let all = ranked;
            out.symbols_dropped = all.len().saturating_sub(SYMBOL_CAP);
            out.symbols = all.into_iter().take(SYMBOL_CAP).collect();
        }
    }
    Ok(out)
}

/// `ns::Class::method` → `method`.
fn last_component(symbol: &str) -> String {
    symbol
        .rsplit("::")
        .next()
        .unwrap_or(symbol)
        .trim()
        .to_string()
}

/// A gitattributes file mapping C/C++ extensions to git's `cpp` diff driver,
/// written once per process into the temp dir (never into the repository).
pub(crate) fn attributes_file() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "byteask-cpp-diff-{}.attributes",
            std::process::id()
        ));
        let mut text = String::new();
        for ext in super::SOURCE_EXTS.iter().chain(super::HEADER_EXTS) {
            text.push_str(&format!("*.{ext} diff=cpp\n"));
        }
        let _ = std::fs::write(&path, text);
        path
    })
    .clone()
}

/// Accumulates symbols with a score.
#[derive(Default)]
struct Found {
    /// name → (score, first-seen order)
    scores: BTreeMap<String, (u32, usize)>,
    order: usize,
    literal_list: Vec<String>,
    lines: BTreeMap<PathBuf, Vec<(u32, u32)>>,
}

impl Found {
    fn add(&mut self, name: &str, score: u32) {
        if !plausible_symbol(name) {
            return;
        }
        self.order += 1;
        let order = self.order;
        let entry = self.scores.entry(name.to_string()).or_insert((0, order));
        entry.0 += score;
    }

    fn ranked(&self) -> Vec<String> {
        let mut all: Vec<(&String, &(u32, usize))> = self.scores.iter().collect();
        all.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
        all.into_iter().map(|(name, _)| name.clone()).collect()
    }

    /// Declarations on one changed line.
    fn declared(&mut self, line: &str) {
        for name in declarations(line) {
            self.add(&name, 3);
        }
    }

    fn literals(&mut self, line: &str) {
        if self.literal_list.len() >= LITERAL_CAP {
            return;
        }
        for lit in string_literals(line) {
            if lit.len() >= 6
                && !self.literal_list.contains(&lit)
                && self.literal_list.len() < LITERAL_CAP
            {
                self.literal_list.push(lit);
            }
        }
    }
}

/// Parse a `git diff -U0` produced with the cpp funcname driver.
fn parse_diff(diff: &str, root: &Path, found: &mut Found) {
    let mut file: Option<PathBuf> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            file = rest.strip_prefix("b/").map(|p| root.join(p));
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("diff --git") || line.starts_with("index ")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let mut parts = rest.splitn(2, " @@");
            let ranges = parts.next().unwrap_or_default();
            let context = parts.next().unwrap_or_default().trim();
            if let (Some(file), Some(range)) = (file.as_ref(), new_side(ranges)) {
                found.lines.entry(file.clone()).or_default().push(range);
            }
            if let Some(name) = enclosing_name(context) {
                found.add(&name, 2);
            }
            continue;
        }
        if let Some(body) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
            found.declared(body);
            found.literals(body);
        }
    }
}

/// `-a,b +c,d` → the new side `(c, c+d-1)`; a pure deletion gives the line it
/// was deleted after (`(c, c)`) so the enclosing test case still counts.
fn new_side(ranges: &str) -> Option<(u32, u32)> {
    let new = ranges.split_whitespace().find(|p| p.starts_with('+'))?;
    let mut parts = new[1..].splitn(2, ',');
    let start: u32 = parts.next()?.parse().ok()?;
    let count: u32 = match parts.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    Some(if count == 0 {
        (start.max(1), start.max(1))
    } else {
        (start, start + count - 1)
    })
}

/// The entity a hunk-header context line names: the identifier before the
/// parameter list of a function, or the name of a class/struct/enum.
pub(crate) fn enclosing_name(context: &str) -> Option<String> {
    let context = context.trim();
    if context.is_empty() || context.starts_with("namespace") || context.starts_with('#') {
        return None;
    }
    if let Some(caps) = type_decl_re().captures(context) {
        return caps.get(2).map(|m| m.as_str().to_string());
    }
    let paren = context.find('(')?;
    let before = context[..paren].trim_end();
    let name: String = before
        .chars()
        .rev()
        .take_while(|c| ident_char(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (!name.is_empty() && !is_keyword(&name)).then_some(name)
}

fn type_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:template\s*<.*>\s*)?(class|struct|union|enum(?:\s+class|\s+struct)?)\s+(?:\[\[[^\]]*\]\]\s*)?(?:[A-Z_]+_EXPORT\s+|[A-Z_]+_API\s+)?([A-Za-z_]\w*)")
            .unwrap_or_else(|_| unreachable_regex())
    })
}

fn func_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // A type (one or more words, qualifiers, pointers, templates), then a
        // possibly qualified name, then `(`. Assignments and calls without a
        // leading type do not match.
        Regex::new(r"^\s*(?:template\s*<[^;{}]*>\s*)?(?:[A-Za-z_][\w:<>,]*[\s\*&]+)+(?:[A-Za-z_]\w*::)*(~?[A-Za-z_]\w*)\s*\(")
            .unwrap_or_else(|_| unreachable_regex())
    })
}

fn misc_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:#\s*define\s+([A-Za-z_]\w*)|\busing\s+([A-Za-z_]\w*)\s*=|\btypedef\b[^;]*?\b([A-Za-z_]\w*)\s*;)")
            .unwrap_or_else(|_| unreachable_regex())
    })
}

/// Only reached if a literal pattern above fails to compile, which a unit test
/// rules out; a regex that matches nothing keeps the tool alive regardless.
fn unreachable_regex() -> Regex {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"[^\s\S]").unwrap()
}

/// Names declared on one line of code.
pub(crate) fn declarations(line: &str) -> Vec<String> {
    let code = strip_line_comment(line);
    let trimmed = code.trim_start();
    let mut out = Vec::new();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
        return out;
    }
    if let Some(caps) = type_decl_re().captures(trimmed) {
        if let Some(name) = caps.get(2) {
            out.push(name.as_str().to_string());
        }
    }
    for caps in misc_decl_re().captures_iter(code) {
        for i in 1..=3 {
            if let Some(name) = caps.get(i) {
                out.push(name.as_str().to_string());
            }
        }
    }
    let first_word: String = trimmed.chars().take_while(|c| ident_char(*c)).collect();
    if !is_statement_keyword(&first_word) {
        if let Some(caps) = func_decl_re().captures(code) {
            if let Some(name) = caps.get(1) {
                let name = name.as_str().trim_start_matches('~');
                out.push(name.to_string());
            }
            // `Type Class::method(` also names the class.
            let text = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            if let Some(idx) = text.rfind("::") {
                let class: String = text[..idx]
                    .chars()
                    .rev()
                    .take_while(|c| ident_char(*c))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if !class.is_empty() {
                    out.push(class);
                }
            }
        }
    }
    out.retain(|n| plausible_symbol(n));
    out
}

fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(idx) if !line[..idx].contains('"') => &line[..idx],
        _ => line,
    }
}

/// String literals on one line (without quotes, escapes kept).
pub(crate) fn string_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '"' {
            continue;
        }
        // Skip a character literal '"' and include-style lines.
        if i > 0 && line[..i].ends_with('\'') {
            continue;
        }
        let mut lit = String::new();
        let mut closed = false;
        while let Some((_, c)) = chars.next() {
            match c {
                '\\' => {
                    lit.push(c);
                    if let Some((_, next)) = chars.next() {
                        lit.push(next);
                    }
                }
                '"' => {
                    closed = true;
                    break;
                }
                other => lit.push(other),
            }
        }
        if closed && !line.trim_start().starts_with("#include") {
            out.push(lit);
        }
    }
    out
}

fn plausible_symbol(name: &str) -> bool {
    name.len() >= 3
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(ident_char)
        && !is_keyword(name)
        && !matches!(name, "main" | "operator" | "std" | "SECTION")
        && !is_test_macro(name)
}

/// A test-framework macro name (never a changed entity).
fn is_test_macro(name: &str) -> bool {
    gtest_re().is_match(&format!("{name}(a, b)"))
        || named_case_re().is_match(&format!("{name}(")) && name.contains("CASE")
}

fn is_statement_keyword(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "if"
            | "for"
            | "while"
            | "switch"
            | "else"
            | "case"
            | "do"
            | "sizeof"
            | "delete"
            | "new"
            | "throw"
            | "co_await"
            | "co_yield"
            | "goto"
            | "static_assert"
            | "assert"
            | "catch"
            | "decltype"
            | "alignof"
            | "typeid"
    )
}

fn is_keyword(word: &str) -> bool {
    is_statement_keyword(word)
        || matches!(
            word,
            "int"
                | "char"
                | "void"
                | "bool"
                | "float"
                | "double"
                | "long"
                | "short"
                | "unsigned"
                | "signed"
                | "const"
                | "constexpr"
                | "consteval"
                | "constinit"
                | "static"
                | "inline"
                | "virtual"
                | "explicit"
                | "extern"
                | "volatile"
                | "mutable"
                | "auto"
                | "register"
                | "struct"
                | "class"
                | "union"
                | "enum"
                | "typedef"
                | "typename"
                | "template"
                | "namespace"
                | "using"
                | "public"
                | "private"
                | "protected"
                | "friend"
                | "this"
                | "true"
                | "false"
                | "nullptr"
                | "NULL"
                | "noexcept"
                | "override"
                | "final"
                | "try"
                | "break"
                | "continue"
                | "default"
                | "size_t"
                | "uint8_t"
                | "uint16_t"
                | "uint32_t"
                | "uint64_t"
                | "int8_t"
                | "int16_t"
                | "int32_t"
                | "int64_t"
                | "string"
                | "vector"
                | "defined"
                | "include"
                | "define"
                | "endif"
                | "ifdef"
                | "ifndef"
                | "elif"
                | "undef"
                | "pragma"
                | "requires"
                | "concept"
        )
}

// ---------------------------------------------------------------------------
// Test-case index
// ---------------------------------------------------------------------------

/// Test framework of a test program or file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Framework {
    GTest,
    Catch,
    Doctest,
    Boost,
}

impl Framework {
    /// What `<program> --help` says about its framework.
    pub(crate) fn from_help(text: &str) -> Option<Self> {
        if text.contains("--gtest_filter") {
            Some(Framework::GTest)
        } else if text.contains("doctest") || text.contains("--test-case=") {
            Some(Framework::Doctest)
        } else if text.contains("--run_test") {
            Some(Framework::Boost)
        } else if text.contains("Catch")
            || (text.contains("--reporter") && text.contains("test spec"))
        {
            Some(Framework::Catch)
        } else {
            None
        }
    }

    /// Can a single case be selected on the command line (supported here)?
    pub(crate) fn filterable(self) -> bool {
        !matches!(self, Framework::Boost)
    }
}

/// One test case in a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TestCase {
    /// The name as the framework's filter selects it (`Suite.Name`, a Catch
    /// or doctest case name, a gtest pattern for parameterised cases).
    pub filter: String,
    /// Display name.
    pub name: String,
    /// Grouping for neighbours: the gtest suite, else the file.
    pub suite: String,
    pub framework: Framework,
    /// 1-based first and last line of the case.
    pub start: u32,
    pub end: u32,
}

fn gtest_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // GoogleTest's test macros (longest names first).
        Regex::new(
            r"^\s*(TYPED_TEST_P|TYPED_TEST|TEST_F|TEST_P|TEST)\s*\(\s*(\w+)\s*,\s*(\w+)\s*\)",
        )
        .unwrap_or_else(|_| unreachable_regex())
    })
}

/// Catch and doctest macros that define a named test case or scenario.
const NAMED_CASE_MACROS: &[&str] = &[
    "TEST_CASE",
    "TEST_CASE_METHOD",
    "TEST_CASE_FIXTURE",
    "TEST_CASE_TEMPLATE",
    "TEST_CASE_TEMPLATE_DEFINE",
    "TEMPLATE_TEST_CASE",
    "TEMPLATE_TEST_CASE_SIG",
    "TEMPLATE_TEST_CASE_METHOD",
    "TEMPLATE_TEST_CASE_METHOD_SIG",
    "TEMPLATE_PRODUCT_TEST_CASE",
    "TEMPLATE_PRODUCT_TEST_CASE_SIG",
    "TEMPLATE_PRODUCT_TEST_CASE_METHOD",
    "TEMPLATE_PRODUCT_TEST_CASE_METHOD_SIG",
    "TEMPLATE_LIST_TEST_CASE",
    "TEMPLATE_LIST_TEST_CASE_METHOD",
    "SCENARIO",
    "SCENARIO_METHOD",
    "SCENARIO_TEMPLATE",
    "SCENARIO_TEMPLATE_DEFINE",
    "SCENARIO_CLASS",
];

fn named_case_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Named cases of Catch and doctest: the test-case and scenario macros
        // with their method/fixture/template forms and CATCH_/DOCTEST_ prefixes.
        let mut names = NAMED_CASE_MACROS.to_vec();
        names.sort_by_key(|name| std::cmp::Reverse(name.len()));
        Regex::new(&format!(
            r"^\s*((?:CATCH_|DOCTEST_)?(?:{}))\s*\(",
            names.join("|")
        ))
        .unwrap_or_else(|_| unreachable_regex())
    })
}

fn boost_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(BOOST_AUTO_TEST_CASE|BOOST_FIXTURE_TEST_CASE|BOOST_DATA_TEST_CASE|BOOST_AUTO_TEST_CASE_TEMPLATE)\s*\(\s*(\w+)")
            .unwrap_or_else(|_| unreachable_regex())
    })
}

/// The test cases defined in one source file, in order.
pub(crate) fn index_cases(text: &str, file_label: &str) -> Vec<TestCase> {
    let doctest = text.contains("doctest");
    let lines: Vec<&str> = text.lines().collect();
    let offsets = line_offsets(text);
    let mut cases = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let start = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let (filter, name, suite, framework) = if let Some(caps) = gtest_re().captures(line) {
            let kind = caps.get(1).map_or("", |m| m.as_str());
            let suite = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let case = caps.get(3).map_or("", |m| m.as_str());
            let typed = kind.starts_with("TYPED");
            let param = kind.ends_with("_P");
            let filter = match (typed, param) {
                (false, true) => format!("*{suite}.{case}/*"),
                (true, false) => format!("{suite}/*.{case}"),
                (true, true) => format!("*{suite}/*.{case}"),
                (false, false) => format!("{suite}.{case}"),
            };
            (filter, format!("{suite}.{case}"), suite, Framework::GTest)
        } else if let Some(caps) = named_case_re().captures(line) {
            let macro_name = caps.get(1).map_or("", |m| m.as_str());
            let window = lines[i..lines.len().min(i + 4)].join("\n");
            let Some(lit) = string_literals(&window).into_iter().next() else {
                continue;
            };
            let lit = c_unescape(&lit);
            if !(macro_name.contains("CASE") || macro_name.contains("SCENARIO")) {
                continue;
            }
            let doctest_form = macro_name.starts_with("DOCTEST")
                || macro_name.ends_with("_FIXTURE")
                || macro_name.ends_with("_TEMPLATE");
            let framework = if doctest || doctest_form {
                Framework::Doctest
            } else {
                Framework::Catch
            };
            let name = if macro_name.starts_with("SCENARIO") {
                match framework {
                    Framework::Doctest => format!("  Scenario: {lit}"),
                    _ => format!("Scenario: {lit}"),
                }
            } else {
                lit
            };
            (name.clone(), name, file_label.to_string(), framework)
        } else if let Some(caps) = boost_re().captures(line) {
            let name = caps.get(2).map_or("", |m| m.as_str()).to_string();
            (name.clone(), name, file_label.to_string(), Framework::Boost)
        } else {
            continue;
        };
        let end = case_end(text, offsets[i], &offsets).unwrap_or(start);
        cases.push(TestCase {
            filter,
            name,
            suite,
            framework,
            start,
            end,
        });
    }
    cases
}

/// The common C escapes in a string literal's text (`\"`, `\\`, `\n`, `\t`).
fn c_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// The last line of the case whose macro starts at byte `from`: the line of the
/// brace that closes the first `{` after the macro, skipping comments, string
/// and character literals.
fn case_end(text: &str, from: usize, offsets: &[usize]) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut i = from;
    let mut depth = 0i32;
    let mut opened = false;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'{' => {
                depth += 1;
                opened = true;
            }
            b'}' => {
                depth -= 1;
                if opened && depth == 0 {
                    let line = offsets.partition_point(|&o| o <= i);
                    return u32::try_from(line).ok();
                }
            }
            b';' if !opened && depth == 0 => {
                // A declaration, not a definition.
                let line = offsets.partition_point(|&o| o <= i);
                return u32::try_from(line).ok();
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Which case (index into `cases`) contains 1-based `line`, if any.
pub(crate) fn case_at(cases: &[TestCase], line: u32) -> Option<usize> {
    cases.iter().position(|c| c.start <= line && line <= c.end)
}

/// Lines (1-based) of `text` mentioning any of `words` as a whole identifier.
pub(crate) fn word_hits(text: &str, words: &[String]) -> Vec<(u32, String)> {
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for word in words {
            if contains_word(line, word) {
                hits.push((u32::try_from(i + 1).unwrap_or(u32::MAX), word.clone()));
            }
        }
    }
    hits
}

/// `word` occurs in `line` with no identifier character on either side.
pub(crate) fn contains_word(line: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = line[start..].find(word) {
        let at = start + pos;
        let before = line[..at].chars().next_back();
        let after = line[at + word.len()..].chars().next();
        if !before.is_some_and(ident_char) && !after.is_some_and(ident_char) {
            return true;
        }
        start = at + word.len();
    }
    false
}

/// `*`/`?` glob match (gtest filter patterns).
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn regexes_compile() {
        assert!(type_decl_re().is_match("class parser {"));
        assert!(func_decl_re().is_match("int twice(int x) {"));
        assert!(misc_decl_re().is_match("#define X 1"));
        assert!(gtest_re().is_match("TEST(A, B) {"));
        assert!(named_case_re().is_match("TEST_CASE(\"x\") {"));
        assert!(named_case_re().is_match("CATCH_TEST_CASE(\"x\") {"));
        assert!(named_case_re().is_match("TEMPLATE_LIST_TEST_CASE_METHOD(F, \"x\", \"\", L) {"));
        assert!(!named_case_re().is_match("TEST_SUITE(\"x\") {"));
        assert!(boost_re().is_match("BOOST_AUTO_TEST_CASE(x)"));
    }

    #[test]
    fn finds_declared_names() {
        assert_eq!(
            declarations("inline std::tm gmtime(std::time_t time) {"),
            vec!["gmtime"]
        );
        assert_eq!(
            declarations("auto Widget::size() const -> std::size_t {"),
            vec!["size", "Widget"]
        );
        assert_eq!(
            declarations("template <typename T> struct formatter<T> {"),
            vec!["formatter"]
        );
        assert_eq!(declarations("#define FMT_THING 1"), vec!["FMT_THING"]);
        assert_eq!(declarations("using handle_t = int;"), vec!["handle_t"]);
        assert!(declarations("  return twice(x);").is_empty());
        assert!(declarations("  auto y = make_thing(1);").is_empty());
        assert!(declarations("  if (ok(x)) {").is_empty());
        assert!(declarations("  foo(bar);").is_empty());
    }

    #[test]
    fn names_the_enclosing_entity() {
        assert_eq!(
            enclosing_name("int twice(int x) {").as_deref(),
            Some("twice")
        );
        assert_eq!(enclosing_name("class parser {").as_deref(), Some("parser"));
        assert_eq!(enclosing_name("namespace detail {"), None);
        assert_eq!(enclosing_name(""), None);
    }

    #[test]
    fn reads_new_side_ranges() {
        assert_eq!(new_side("-3,0 +4,2"), Some((4, 5)));
        assert_eq!(new_side("-3 +3"), Some((3, 3)));
        assert_eq!(new_side("-3,2 +2,0"), Some((2, 2)));
    }

    #[test]
    fn collects_symbols_and_literals_from_a_diff() {
        let diff = "diff --git a/src/core.cpp b/src/core.cpp\n\
--- a/src/core.cpp\n\
+++ b/src/core.cpp\n\
@@ -5 +5 @@ int twice(int x) {\n\
-  return x * 2;\n\
+  return x + x; // twice\n\
@@ -9,0 +10,3 @@ int twice(int x) {\n\
+int thrice(int x) {\n\
+  throw error(\"value out of range\");\n\
+}\n";
        let mut found = Found::default();
        parse_diff(diff, Path::new("/r"), &mut found);
        assert_eq!(found.ranked(), vec!["twice", "thrice"]);
        assert_eq!(found.literal_list, vec!["value out of range"]);
        assert_eq!(
            found.lines.get(Path::new("/r/src/core.cpp")),
            Some(&vec![(5, 5), (10, 12)])
        );
    }

    #[test]
    fn indexes_test_cases_of_each_framework() {
        let gtest = concat!(
            "#include \"gtest/gtest.h\"\n\
TEST(Core, Twice) {\n  EXPECT_EQ(twice(2), 4);\n}\n\
static int helper() { return 1; }\n\
TEST_P(Param, Works) {\n  // {\n  EXPECT_EQ(1, 1);\n}\n\
TYPED_TEST(Typed, Size) { }\n"
        )
        .to_string();
        let cases = index_cases(&gtest, "t.cc");
        let summary: Vec<(String, u32, u32)> = cases
            .iter()
            .map(|c| (c.filter.clone(), c.start, c.end))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Core.Twice".to_string(), 2, 4),
                ("*Param.Works/*".to_string(), 6, 9),
                ("Typed/*.Size".to_string(), 10, 10),
            ]
        );
        assert_eq!(case_at(&cases, 3), Some(0));
        assert_eq!(case_at(&cases, 5), None);

        let catch = "#include \"catch.hpp\"\nTEST_CASE(\"parses \\\"x\\\"\", \"[p]\") {\n  REQUIRE(1);\n}\nSCENARIO(\"s\") {}\n";
        let cases = index_cases(&catch, "c.cc");
        assert_eq!(cases[0].filter, "parses \"x\"");
        assert_eq!(cases[0].framework, Framework::Catch);
        assert_eq!(cases[1].filter, "Scenario: s");

        let doctest = "#include \"doctest.h\"\nTEST_CASE(\"dt\") {\n}\n";
        assert_eq!(
            index_cases(doctest, "d.cc")[0].framework,
            Framework::Doctest
        );

        let boost = "BOOST_AUTO_TEST_CASE(my_case) {\n}\n";
        assert_eq!(index_cases(boost, "b.cc")[0].framework, Framework::Boost);
    }

    #[test]
    fn word_matching_and_globs() {
        assert!(contains_word("EXPECT_EQ(twice(2), 4);", "twice"));
        assert!(!contains_word("twicer(2)", "twice"));
        assert!(!contains_word("my_twice(2)", "twice"));
        assert!(glob_match("*Param.Works/*", "Inst/Param.Works/3"));
        assert!(glob_match("Core.Twice", "Core.Twice"));
        assert!(!glob_match("Core.Twice", "Core.Twice2"));
        assert!(glob_match("Typed/*.Size", "Typed/0.Size"));
    }

    #[test]
    fn detects_frameworks_from_help() {
        assert_eq!(
            Framework::from_help("  --gtest_filter=POSITIVE"),
            Some(Framework::GTest)
        );
        assert_eq!(
            Framework::from_help("[doctest] ..."),
            Some(Framework::Doctest)
        );
        assert_eq!(
            Framework::from_help("Catch v2.13.1\nusage:"),
            Some(Framework::Catch)
        );
        assert_eq!(Framework::from_help("usage: prog"), None);
    }

    #[tokio::test]
    async fn reads_changes_from_git() {
        use crate::session_head::test_repo;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/core.cpp"),
            "int twice(int x) {\n  return x * 2;\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        let base = test_repo::init_and_commit(root);
        std::fs::write(
            root.join("src/core.cpp"),
            "int twice(int x) {\n  return x + x;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/extra.cpp"),
            "int extra_fn(int y) { return y; }\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "untracked, not code\n").unwrap();
        let changes = changes(root, &base, "base".to_string(), None, None)
            .await
            .unwrap();
        let rel: Vec<String> = changes
            .files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().display().to_string())
            .collect();
        assert_eq!(rel, vec!["src/core.cpp", "src/extra.cpp"]);
        assert_eq!(changes.symbols, vec!["extra_fn", "twice"]);
        assert_eq!(
            changes.lines.get(&root.join("src/core.cpp")),
            Some(&vec![(2, 2)])
        );
    }
}
