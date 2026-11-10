//! A small, dependency-free C/C++ scanner: tokens, declarations with their spans, `switch`
//! statements with their `case` labels, and the `#if` branch structure.
//!
//! It is the textual tier of `cpp_impact`: it never needs a compile database and works on
//! every compiler the images ship. It is heuristic by design (macros are not expanded, types
//! are not resolved) and every item it produces is labelled textual unless clangd confirms it.
//!
//! Conditional compilation: each `#if` group's branches are scanned from the same starting
//! state (the scope stack is saved at `#if` and restored at `#elif`/`#else`), and scanning
//! continues after `#endif` from the state at the end of the FIRST branch. So a declaration
//! written twice, once per configuration, yields two entities with the same name and
//! different branches, and unbalanced braces across branches do not derail the scan.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokKind {
    Ident,
    Number,
    Str,
    Char,
    Punct,
    /// `#if`/`#ifdef`/`#ifndef` (opens a group), `#elif`/`#else`, `#endif`.
    If,
    Else,
    Endif,
}

#[derive(Clone, Debug)]
pub(crate) struct Tok {
    pub kind: TokKind,
    pub text: String,
    pub line: u32,
}

/// One preprocessor directive (continuation lines joined).
#[derive(Clone, Debug)]
pub(crate) struct Directive {
    pub line: u32,
    pub end_line: u32,
    /// Directive keyword without `#` (`if`, `ifdef`, `define`, …).
    pub keyword: String,
    /// Rest of the directive, whitespace-collapsed, comments removed.
    pub rest: String,
}

/// Tokenize `src`. Comments are dropped; preprocessor directives are returned separately and
/// only their conditional structure appears in the token stream.
pub(crate) fn tokenize(src: &str) -> (Vec<Tok>, Vec<Directive>) {
    let bytes = src.as_bytes();
    let mut toks = Vec::new();
    let mut directives = Vec::new();
    let mut i = 0usize;
    let mut line: u32 = 1;
    let mut at_line_start = true;
    let n = bytes.len();
    while i < n {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            at_line_start = true;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' || c == 0x0c || c == 0x0b {
            i += 1;
            continue;
        }
        if c == b'\\' && i + 1 < n && (bytes[i + 1] == b'\n' || bytes[i + 1] == b'\r') {
            // Stray line continuation outside a directive.
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            i += 2;
            while i < n && !(bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if c == b'#' && at_line_start {
            let start_line = line;
            let mut text: Vec<u8> = Vec::new();
            i += 1;
            while i < n {
                let b = bytes[i];
                if b == b'\\' && i + 1 < n && bytes[i + 1] == b'\n' {
                    line += 1;
                    i += 2;
                    text.push(b' ');
                    continue;
                }
                if b == b'\\' && i + 2 < n && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                    line += 1;
                    i += 3;
                    text.push(b' ');
                    continue;
                }
                if b == b'\n' {
                    break;
                }
                if b == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
                    while i < n && bytes[i] != b'\n' {
                        i += 1;
                    }
                    break;
                }
                if b == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    i += 2;
                    while i < n && !(bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/') {
                        if bytes[i] == b'\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                    i = (i + 2).min(n);
                    text.push(b' ');
                    continue;
                }
                text.push(b);
                i += 1;
            }
            let text = String::from_utf8_lossy(&text)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let (keyword, rest) = match text.split_once(' ') {
                Some((k, r)) => (k.to_string(), r.to_string()),
                None => (text.clone(), String::new()),
            };
            let kind = match keyword.as_str() {
                "if" | "ifdef" | "ifndef" => Some(TokKind::If),
                "elif" | "else" | "elifdef" | "elifndef" => Some(TokKind::Else),
                "endif" => Some(TokKind::Endif),
                _ => None,
            };
            if let Some(kind) = kind {
                toks.push(Tok {
                    kind,
                    text: keyword.clone(),
                    line: start_line,
                });
            }
            directives.push(Directive {
                line: start_line,
                end_line: line,
                keyword,
                rest,
            });
            continue;
        }
        at_line_start = false;
        // Raw string literal R"delim( ... )delim" (with optional prefix u8/u/U/L).
        if (c == b'R' || ((c == b'u' || c == b'U' || c == b'L') && raw_prefix(bytes, i)))
            && let Some(end) = raw_string_end(bytes, i)
        {
            let start_line = line;
            for &b in &bytes[i..end] {
                if b == b'\n' {
                    line += 1;
                }
            }
            toks.push(Tok {
                kind: TokKind::Str,
                text: "\"\"".to_string(),
                line: start_line,
            });
            i = end;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let quote = c;
            let start_line = line;
            let start = i;
            i += 1;
            while i < n && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < n {
                    if bytes[i + 1] == b'\n' {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\n' {
                    // Unterminated literal: stop at the line end.
                    break;
                }
                i += 1;
            }
            let end = (i + 1).min(n);
            let text = String::from_utf8_lossy(&bytes[start..end]).into_owned();
            toks.push(Tok {
                kind: if quote == b'"' {
                    TokKind::Str
                } else {
                    TokKind::Char
                },
                text: truncate_literal(text),
                line: start_line,
            });
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < n
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80)
            {
                i += 1;
            }
            // String prefix like u8"..." / L'x' is folded into the literal on the next token.
            toks.push(Tok {
                kind: TokKind::Ident,
                text: String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                line,
            });
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < n && bytes[i + 1].is_ascii_digit()) {
            let start = i;
            while i < n
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'.'
                    || bytes[i] == b'_'
                    || (bytes[i] == b'\'' && i + 1 < n && bytes[i + 1].is_ascii_alphanumeric())
                    || ((bytes[i] == b'+' || bytes[i] == b'-')
                        && matches!(bytes[i - 1], b'e' | b'E' | b'p' | b'P')))
            {
                i += 1;
            }
            toks.push(Tok {
                kind: TokKind::Number,
                text: String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                line,
            });
            continue;
        }
        // Punctuation: a few multi-character tokens matter for parsing.
        let two = if i + 1 < n {
            &bytes[i..i + 2]
        } else {
            &bytes[i..i + 1]
        };
        let text = if i + 2 < n && &bytes[i..i + 3] == b"..." {
            "...".to_string()
        } else if two == b"::" || two == b"->" {
            String::from_utf8_lossy(two).into_owned()
        } else {
            (c as char).to_string()
        };
        i += text.len();
        toks.push(Tok {
            kind: TokKind::Punct,
            text,
            line,
        });
    }
    (toks, directives)
}

fn raw_prefix(bytes: &[u8], i: usize) -> bool {
    // u8R" / uR" / UR" / LR"
    let rest = &bytes[i..];
    rest.starts_with(b"u8R\"")
        || rest.starts_with(b"uR\"")
        || rest.starts_with(b"UR\"")
        || rest.starts_with(b"LR\"")
}

fn raw_string_end(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j < bytes.len() && bytes[j] != b'R' {
        j += 1;
        if j - i > 3 {
            return None;
        }
    }
    if j + 1 >= bytes.len() || bytes[j + 1] != b'"' {
        return None;
    }
    // A preceding identifier character means this R is part of an identifier (e.g. `FOR"`).
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
    let delim_start = j + 2;
    let mut k = delim_start;
    while k < bytes.len() && bytes[k] != b'(' {
        if k - delim_start > 16 || bytes[k] == b'\n' || bytes[k] == b' ' {
            return None;
        }
        k += 1;
    }
    let delim = &bytes[delim_start..k];
    let mut close = Vec::with_capacity(delim.len() + 2);
    close.push(b')');
    close.extend_from_slice(delim);
    close.push(b'"');
    let body = k + 1;
    bytes[body..]
        .windows(close.len())
        .position(|window| window == close.as_slice())
        .map(|pos| body + pos + close.len())
}

fn truncate_literal(text: String) -> String {
    if text.len() > 80 {
        let mut cut = 77;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &text[..cut])
    } else {
        text
    }
}

// ---------------------------------------------------------------------------------------------
// Conditional-compilation branches
// ---------------------------------------------------------------------------------------------

/// One branch of an `#if` group.
#[derive(Clone, Debug)]
pub(crate) struct Branch {
    pub group: usize,
    /// 0 for the `#if`, then 1.. for each `#elif`/`#else`.
    pub index: usize,
    /// The directive text: `if X`, `ifdef X`, `elif Y`, `else`.
    pub condition: String,
    /// First and last line of the branch body (between its directive and the next one).
    pub start: u32,
    pub end: u32,
    /// The whole group is an include guard (`#ifndef X` + `#define X`, no `#else`).
    pub guard: bool,
}

/// The branch structure of a file's `#if` groups.
pub(crate) fn branches(directives: &[Directive]) -> Vec<Branch> {
    let mut out: Vec<Branch> = Vec::new();
    // Stack of (group id, index of the open branch in `out`).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut next_group = 0usize;
    for (pos, directive) in directives.iter().enumerate() {
        match directive.keyword.as_str() {
            "if" | "ifdef" | "ifndef" => {
                let group = next_group;
                next_group += 1;
                let guard = directive.keyword == "ifndef"
                    && directives.get(pos + 1).is_some_and(|next| {
                        next.keyword == "define"
                            && next.rest.split_whitespace().next()
                                == directive.rest.split_whitespace().next()
                    });
                out.push(Branch {
                    group,
                    index: 0,
                    condition: format!("{} {}", directive.keyword, directive.rest),
                    start: directive.end_line + 1,
                    end: u32::MAX,
                    guard,
                });
                stack.push((group, out.len() - 1));
            }
            "elif" | "else" | "elifdef" | "elifndef" => {
                if let Some((group, open)) = stack.pop() {
                    out[open].end = directive.line.saturating_sub(1);
                    let index = out[open].index + 1;
                    let guard = out[open].guard;
                    out.push(Branch {
                        group,
                        index,
                        condition: format!("{} {}", directive.keyword, directive.rest)
                            .trim()
                            .to_string(),
                        start: directive.end_line + 1,
                        end: u32::MAX,
                        guard,
                    });
                    stack.push((group, out.len() - 1));
                }
            }
            "endif" => {
                if let Some((group, open)) = stack.pop() {
                    out[open].end = directive.line.saturating_sub(1);
                    // A group with more than one branch is not an include guard.
                    if out.iter().filter(|b| b.group == group).count() > 1 {
                        for branch in out.iter_mut().filter(|b| b.group == group) {
                            branch.guard = false;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Branches (innermost last) that contain `line`, include guards excluded.
pub(crate) fn branches_at(branches: &[Branch], line: u32) -> Vec<&Branch> {
    branches
        .iter()
        .filter(|b| !b.guard && b.start <= line && line <= b.end)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntityKind {
    Function,
    Class,
    Enum,
}

#[derive(Clone, Debug)]
pub(crate) struct Entity {
    pub kind: EntityKind,
    /// Unqualified name: `size`, `operator<<`, `~Widget`, `Widget`.
    pub name: String,
    /// Enclosing scopes plus any qualification written on the declarator, `::`-joined
    /// (`lib::detail::Widget` for a member of Widget, `lib` for a free function).
    pub scope: String,
    /// Parameter list text (functions), whitespace-normalised, without parentheses.
    pub params: String,
    /// One normalised type per parameter (names and default arguments dropped).
    pub param_types: Vec<String>,
    /// Qualifiers after the parameter list: `const`, `&&`, `noexcept`, `override`, …
    pub trailing: String,
    /// `template <…>` header text when present (empty for `template <>`).
    pub template: Option<String>,
    /// Class specialisation arguments: `<std::queue<T>, Char>` for `struct formatter<…>`.
    pub spec_args: Option<String>,
    /// Base classes (last component of each name).
    pub bases: Vec<String>,
    pub is_virtual: bool,
    /// Has a body (definition) rather than being a declaration.
    pub is_def: bool,
    /// First line of the declaration (template header included) and the closing line.
    pub line: u32,
    pub end_line: u32,
    /// Line of the name itself.
    pub name_line: u32,
    /// Token range of the body (exclusive of braces) for functions with a body.
    pub body: Option<(usize, usize)>,
    /// Nothing precedes the name (no return type, no qualification): a constructor, a
    /// destructor, or a macro invocation such as `TEST(a, b)`.
    pub bare: bool,
}

impl Entity {
    /// `scope::name` (or just `name`).
    pub fn qualified(&self) -> String {
        if self.scope.is_empty() {
            self.name.clone()
        } else {
            format!("{}::{}", self.scope, self.name)
        }
    }

    /// A short signature for rendering: `scope::name(params) trailing`.
    pub fn signature(&self) -> String {
        match self.kind {
            EntityKind::Function => {
                let mut sig = format!("{}({})", self.qualified(), self.params);
                if !self.trailing.is_empty() {
                    sig.push(' ');
                    sig.push_str(&self.trailing);
                }
                sig
            }
            EntityKind::Class => {
                let mut sig = format!("class {}", self.qualified());
                if let Some(args) = &self.spec_args {
                    sig.push_str(args);
                }
                sig
            }
            EntityKind::Enum => format!("enum {}", self.qualified()),
        }
    }

    /// Identity used to match the same entity across two versions of a file.
    pub fn key(&self) -> String {
        format!(
            "{:?}|{}|{}|{}|{}",
            self.kind,
            self.qualified(),
            self.param_types.join(","),
            self.trailing,
            self.spec_args.clone().unwrap_or_default()
        )
    }

    /// The class this entity is a member of (last scope component), for members.
    pub fn owner(&self) -> Option<&str> {
        self.scope.rsplit("::").next().filter(|s| !s.is_empty())
    }
}

/// A `switch` statement with its labels.
#[derive(Clone, Debug)]
pub(crate) struct Switch {
    pub expr: String,
    pub line: u32,
    pub end_line: u32,
    /// (label text, line of the label, last line of the code that follows it).
    pub cases: Vec<(String, u32, u32)>,
}

/// Everything the scanner found in one file.
#[derive(Clone, Debug, Default)]
pub(crate) struct FileScan {
    pub toks: Vec<Tok>,
    pub branches: Vec<Branch>,
    pub entities: Vec<Entity>,
    pub switches: Vec<Switch>,
}

impl FileScan {
    /// The innermost entity whose span contains `line` (functions preferred over classes).
    pub fn entity_at(&self, line: u32) -> Option<&Entity> {
        self.entities
            .iter()
            .filter(|e| e.line <= line && line <= e.end_line)
            .min_by_key(|e| (e.end_line - e.line, matches!(e.kind, EntityKind::Class)))
    }

    /// Normalised identifier/punctuation tokens of a function body, literals folded.
    pub fn body_tokens(&self, entity: &Entity) -> Vec<&str> {
        let Some((a, b)) = entity.body else {
            return Vec::new();
        };
        self.toks[a..b]
            .iter()
            .filter(|t| !matches!(t.kind, TokKind::If | TokKind::Else | TokKind::Endif))
            .map(|t| match t.kind {
                TokKind::Number => "0",
                TokKind::Str => "\"\"",
                TokKind::Char => "''",
                _ => t.text.as_str(),
            })
            .collect()
    }
}

pub(crate) fn scan(src: &str) -> FileScan {
    let (toks, directives) = tokenize(src);
    let branches = branches(&directives);
    let mut parser = Parser::new(&toks);
    parser.run();
    let last_line = toks.last().map(|t| t.line).unwrap_or(1);
    let mut entities = parser.entities;
    for entity in &mut entities {
        if entity.end_line == u32::MAX {
            // Never closed (unbalanced braces across configurations): keep only its head.
            entity.end_line = entity.name_line.max(entity.line).min(last_line);
            entity.body = None;
        }
    }
    let switches = find_switches(&toks, &entities);
    FileScan {
        toks,
        branches,
        entities,
        switches,
    }
}

#[derive(Clone, Debug)]
enum Scope {
    Namespace(String),
    Class(String),
    /// Function body: index into `entities` of the function.
    Function(usize),
    /// Anything else (initializer, enum body, extern "C" handled as Namespace("")).
    Block,
}

#[derive(Clone)]
struct ParserState {
    scopes: Vec<Scope>,
    stmt: Vec<usize>,
    /// Token index where the current statement started.
    stmt_start_line: Option<u32>,
}

struct Parser<'a> {
    toks: &'a [Tok],
    state: ParserState,
    /// Saved states for open `#if` groups: (state at #if, state at end of first branch).
    cond_stack: Vec<(ParserState, Option<ParserState>)>,
    entities: Vec<Entity>,
}

const NOT_A_NAME: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "return",
    "sizeof",
    "alignof",
    "alignas",
    "decltype",
    "noexcept",
    "throw",
    "catch",
    "static_assert",
    "requires",
    "typeof",
    "__typeof__",
    "__attribute__",
    "__declspec",
    "defined",
    "new",
    "delete",
    "case",
    "do",
    "else",
    "typeid",
    "__asm__",
    "asm",
    "_Pragma",
    "__pragma",
    "_Static_assert",
    "using",
    "typedef",
    "goto",
    "explicit",
];

impl<'a> Parser<'a> {
    fn new(toks: &'a [Tok]) -> Self {
        Self {
            toks,
            state: ParserState {
                scopes: Vec::new(),
                stmt: Vec::new(),
                stmt_start_line: None,
            },
            cond_stack: Vec::new(),
            entities: Vec::new(),
        }
    }

    fn in_function(&self) -> bool {
        self.state
            .scopes
            .iter()
            .any(|s| matches!(s, Scope::Function(_) | Scope::Block))
    }

    fn scope_path(&self) -> String {
        self.state
            .scopes
            .iter()
            .filter_map(|s| match s {
                Scope::Namespace(n) | Scope::Class(n) if !n.is_empty() => Some(n.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("::")
    }

    fn run(&mut self) {
        let mut i = 0;
        while i < self.toks.len() {
            let tok = &self.toks[i];
            match tok.kind {
                TokKind::If => {
                    self.cond_stack.push((self.state.clone(), None));
                    i += 1;
                    continue;
                }
                TokKind::Else => {
                    if let Some((at_if, first_end)) = self.cond_stack.last_mut() {
                        if first_end.is_none() {
                            *first_end = Some(self.state.clone());
                        }
                        self.state = at_if.clone();
                    }
                    i += 1;
                    continue;
                }
                TokKind::Endif => {
                    if let Some((_, first_end)) = self.cond_stack.pop()
                        && let Some(first_end) = first_end
                    {
                        // Continue from the first branch's end state, but keep the bodies of
                        // functions closed in later branches closed: only the scope stack
                        // depth matters here.
                        self.state = first_end;
                    }
                    i += 1;
                    continue;
                }
                _ => {}
            }
            if self.in_function() {
                // Inside a body: only track braces.
                match tok.text.as_str() {
                    "{" if tok.kind == TokKind::Punct => self.state.scopes.push(Scope::Block),
                    "}" if tok.kind == TokKind::Punct => self.close_scope(i),
                    _ => {}
                }
                i += 1;
                continue;
            }
            if tok.kind == TokKind::Punct {
                match tok.text.as_str() {
                    ";" => {
                        self.end_statement(i, false);
                        i += 1;
                        continue;
                    }
                    "{" => {
                        if self.initializer_brace() {
                            // Part of the statement (brace-init in a ctor initializer, or an
                            // `= {…}` initializer): skip to the matching brace.
                            let close = matching(self.toks, i, "{", "}");
                            for k in i..=close.min(self.toks.len() - 1) {
                                self.state.stmt.push(k);
                            }
                            i = close + 1;
                            continue;
                        }
                        self.open_brace(i);
                        i += 1;
                        continue;
                    }
                    "}" => {
                        self.state.stmt.clear();
                        self.state.stmt_start_line = None;
                        self.close_scope(i);
                        i += 1;
                        continue;
                    }
                    ":" => {
                        // Access specifier inside a class resets the statement.
                        if let Some(&last) = self.state.stmt.last()
                            && self.state.stmt.len() == 1
                            && matches!(
                                self.toks[last].text.as_str(),
                                "public" | "private" | "protected"
                            )
                        {
                            self.state.stmt.clear();
                            self.state.stmt_start_line = None;
                            i += 1;
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            if self.state.stmt.is_empty() {
                self.state.stmt_start_line = Some(tok.line);
            }
            self.state.stmt.push(i);
            i += 1;
        }
    }

    /// Is the `{` about to be read part of the current statement rather than a scope?
    fn initializer_brace(&self) -> bool {
        let stmt = &self.state.stmt;
        let Some(&last) = stmt.last() else {
            return false;
        };
        let prev = &self.toks[last];
        // `= {`, `return {`, `, {` in an initializer list, `( {`.
        if prev.kind == TokKind::Punct && matches!(prev.text.as_str(), "=" | "," | "(" | "[") {
            // `,` or `(` directly after a ctor-initializer entry is still an initializer.
            return true;
        }
        // Constructor initializer list: `name(args) : a{1}, b(2) {`. Inside it, a brace after
        // an identifier (or `>`) initialises a member; the body brace follows `)` or `}`.
        if let Some(colon) = ctor_init_colon(self.toks, stmt) {
            if last > colon && (prev.kind == TokKind::Ident || prev.text == ">") {
                return true;
            }
        }
        false
    }

    fn open_brace(&mut self, brace: usize) {
        let stmt = std::mem::take(&mut self.state.stmt);
        let start_line = self.state.stmt_start_line.take();
        let scope = self.classify_brace(&stmt, brace, start_line);
        self.state.scopes.push(scope);
    }

    fn close_scope(&mut self, brace: usize) {
        if let Some(scope) = self.state.scopes.pop() {
            match scope {
                Scope::Function(idx) => {
                    if let Some(entity) = self.entities.get_mut(idx) {
                        entity.end_line = self.toks[brace].line;
                        if let Some((start, _)) = entity.body {
                            entity.body = Some((start, brace));
                        }
                    }
                }
                Scope::Class(_) => {
                    // Close the most recent open class entity at this depth.
                    let line = self.toks[brace].line;
                    if let Some(entity) = self
                        .entities
                        .iter_mut()
                        .rev()
                        .find(|e| e.kind == EntityKind::Class && e.end_line == u32::MAX)
                    {
                        entity.end_line = line;
                    }
                }
                _ => {}
            }
        }
    }

    fn end_statement(&mut self, _semi: usize, _brace: bool) {
        let stmt = std::mem::take(&mut self.state.stmt);
        let start_line = self.state.stmt_start_line.take();
        if stmt.is_empty() {
            return;
        }
        if let Some(mut entity) = self.function_declarator(&stmt, start_line) {
            // A declaration only when the parameter list looks like one (a variable with a
            // constructor call, `T x("a", 1);`, has literals in it).
            let owner = self.state.scopes.iter().rev().find_map(|s| match s {
                Scope::Class(n) => Some(n.clone()),
                _ => None,
            });
            let ctor_like =
                entity.name.starts_with('~') || owner.as_deref() == Some(entity.name.as_str());
            if looks_like_param_list(&entity.params) && (!entity.bare || ctor_like) {
                entity.is_def = false;
                entity.end_line = self.toks[*stmt.last().unwrap_or(&0)].line;
                self.entities.push(entity);
            }
        } else if let Some(class) = self.class_head(&stmt, start_line, false) {
            // Forward declaration `class X;` is not recorded.
            let _ = class;
        }
    }

    fn classify_brace(&mut self, stmt: &[usize], brace: usize, start_line: Option<u32>) -> Scope {
        if stmt.is_empty() {
            return Scope::Block;
        }
        let texts: Vec<&str> = stmt.iter().map(|&k| self.toks[k].text.as_str()).collect();
        // extern "C" {
        if texts.len() == 2 && texts[0] == "extern" && self.toks[stmt[1]].kind == TokKind::Str {
            return Scope::Namespace(String::new());
        }
        // namespace a::b {   /  inline namespace v1 {  /  namespace {
        if let Some(pos) = texts.iter().position(|t| *t == "namespace")
            && !texts.contains(&"(")
            && !texts.contains(&"=")
        {
            let name: String = texts[pos + 1..]
                .iter()
                .filter(|t| **t != "inline" && !t.starts_with('['))
                .copied()
                .collect::<Vec<_>>()
                .join("");
            return Scope::Namespace(name.trim_matches(':').to_string());
        }
        if !texts.contains(&"operator") {
            // `T x = {`, `auto f = [](){` at namespace scope: an `=` before any `(`, outside
            // a template header.
            let (_, rest) = split_template_header(self.toks, stmt);
            let mut depth = 0i32;
            for &k in rest {
                match self.toks[k].text.as_str() {
                    "(" if depth == 0 => break,
                    "<" | "[" => depth += 1,
                    ">" | "]" => depth -= 1,
                    "=" if depth <= 0 => return Scope::Block,
                    _ => {}
                }
            }
        }
        if let Some(entity) = self.class_head(stmt, start_line, true) {
            let name = entity.name.clone();
            let is_enum = entity.kind == EntityKind::Enum;
            let mut entity = entity;
            entity.end_line = u32::MAX;
            if is_enum {
                entity.end_line = self.toks[matching(self.toks, brace, "{", "}")].line;
                self.entities.push(entity);
                return Scope::Block;
            }
            self.entities.push(entity);
            return Scope::Class(name);
        }
        if let Some(mut entity) = self.function_declarator(stmt, start_line) {
            entity.is_def = true;
            entity.body = Some((brace + 1, brace + 1));
            entity.end_line = u32::MAX;
            self.entities.push(entity);
            return Scope::Function(self.entities.len() - 1);
        }
        Scope::Block
    }

    /// Parse `[template<…>] class|struct|union|enum [attrs] Name[<args>] [final] [: bases]`.
    fn class_head(
        &self,
        stmt: &[usize],
        start_line: Option<u32>,
        has_body: bool,
    ) -> Option<Entity> {
        let (template, rest) = split_template_header(self.toks, stmt);
        let texts: Vec<&str> = rest.iter().map(|&k| self.toks[k].text.as_str()).collect();
        let kw = texts
            .iter()
            .position(|t| matches!(*t, "class" | "struct" | "union" | "enum"))?;
        if texts[..kw]
            .iter()
            .any(|t| matches!(*t, "(" | "=" | "friend" | "typedef" | "using"))
        {
            return None;
        }
        let is_enum = texts[kw] == "enum";
        let mut j = kw + 1;
        if is_enum && j < texts.len() && matches!(texts[j], "class" | "struct") {
            j += 1;
        }
        // Name: the last identifier (or ::-qualified run) before `<`, `:`, `final`, `{`/end.
        let mut name = String::new();
        let mut name_line = 0;
        while j < texts.len() {
            let t = texts[j];
            if t == "[" {
                // [[attribute]]
                let close = matching_in(self.toks, rest, j, "[", "]");
                j = close + 1;
                continue;
            }
            if t == "alignas" || t == "__attribute__" || t == "__declspec" {
                if texts.get(j + 1) == Some(&"(") {
                    j = matching_in(self.toks, rest, j + 1, "(", ")") + 1;
                    continue;
                }
            }
            if self.toks[rest[j]].kind == TokKind::Ident && t != "final" {
                name = t.to_string();
                name_line = self.toks[rest[j]].line;
                j += 1;
                // Qualified name `ns::X`.
                while texts.get(j) == Some(&"::") && texts.get(j + 1).is_some() {
                    name = texts[j + 1].to_string();
                    j += 2;
                }
                continue;
            }
            break;
        }
        if name.is_empty() {
            return None;
        }
        let mut spec_args = None;
        if texts.get(j) == Some(&"<") {
            let close = matching_angle(self.toks, rest, j);
            spec_args = Some(join_tokens(self.toks, &rest[j..=close.min(rest.len() - 1)]));
            j = close + 1;
        }
        if texts.get(j) == Some(&"final") {
            j += 1;
        }
        let mut bases = Vec::new();
        if texts.get(j) == Some(&":") {
            // Base clause: split on top-level commas, keep the last identifier before `<`.
            let mut depth = 0i32;
            let mut current_last = String::new();
            for (offset, t) in texts[j + 1..].iter().enumerate() {
                match *t {
                    "<" | "(" => depth += 1,
                    ">" | ")" => depth -= 1,
                    "," if depth == 0 => {
                        if !current_last.is_empty() {
                            bases.push(std::mem::take(&mut current_last));
                        }
                        continue;
                    }
                    _ => {}
                }
                if depth == 0
                    && self.toks[rest[j + 1 + offset]].kind == TokKind::Ident
                    && !matches!(*t, "public" | "private" | "protected" | "virtual")
                {
                    current_last = t.to_string();
                }
            }
            if !current_last.is_empty() {
                bases.push(current_last);
            }
        } else if j < texts.len() && has_body {
            // Something else between the name and the brace: not a class head.
            return None;
        }
        let line = start_line.unwrap_or(name_line);
        Some(Entity {
            kind: if is_enum {
                EntityKind::Enum
            } else {
                EntityKind::Class
            },
            name,
            scope: self.scope_path(),
            params: String::new(),
            param_types: Vec::new(),
            trailing: String::new(),
            template,
            spec_args,
            bases,
            is_virtual: false,
            is_def: has_body,
            line,
            end_line: line,
            name_line,
            body: None,
            bare: false,
        })
    }

    /// Parse a function declarator out of a statement: `… name ( params ) trailing`.
    fn function_declarator(&self, stmt: &[usize], start_line: Option<u32>) -> Option<Entity> {
        let (template, rest) = split_template_header(self.toks, stmt);
        if rest.is_empty() {
            return None;
        }
        let texts: Vec<&str> = rest.iter().map(|&k| self.toks[k].text.as_str()).collect();
        if matches!(texts[0], "typedef" | "using" | "static_assert" | "return")
            || texts.contains(&"namespace")
        {
            return None;
        }
        // Locate the parameter list's `(`.
        let mut open = None;
        let mut name = String::new();
        let mut name_pos = 0;
        let mut j = 0;
        while j < texts.len() {
            let t = texts[j];
            if t == "operator" {
                // operator() / operator[] / operator<< / operator bool / operator new
                let mut k = j + 1;
                let mut op = String::from("operator");
                if texts.get(k) == Some(&"(") && texts.get(k + 1) == Some(&")") {
                    op.push_str("()");
                    k += 2;
                } else {
                    while k < texts.len() && texts[k] != "(" {
                        let piece = texts[k];
                        if self.toks[rest[k]].kind == TokKind::Ident
                            && op.len() > "operator".len()
                            && op
                                .chars()
                                .last()
                                .is_some_and(|c| c.is_alphanumeric() || c == '_')
                        {
                            op.push(' ');
                        } else if self.toks[rest[k]].kind == TokKind::Ident {
                            op.push(' ');
                        }
                        op.push_str(piece);
                        k += 1;
                    }
                }
                if texts.get(k) == Some(&"(") {
                    open = Some(k);
                    // Normalise `operator <<` spacing: symbols glued, words spaced.
                    name = normalise_operator(op.trim());
                    name_pos = j;
                }
                break;
            }
            if t == "<" {
                // Skip template arguments in the return type / qualified name.
                j = matching_angle(self.toks, rest, j) + 1;
                continue;
            }
            if t == "[" {
                j = matching_in(self.toks, rest, j, "[", "]") + 1;
                continue;
            }
            if t == "(" {
                let prev = j
                    .checked_sub(1)
                    .map(|p| (texts[p], self.toks[rest[p]].kind));
                match prev {
                    Some((p, TokKind::Ident)) if !NOT_A_NAME.contains(&p) => {
                        // `__attribute__((…))`-like or macro-with-arguments before the real
                        // name: if another `ident (` follows at depth 0, prefer the later one
                        // unless this looks like a declarator already followed by trailing
                        // qualifiers or a body.
                        let close = matching_in(self.toks, rest, j, "(", ")");
                        let after = texts.get(close + 1).copied();
                        let later_call = texts
                            .get(close + 1..)
                            .unwrap_or(&[])
                            .iter()
                            .enumerate()
                            .any(|(o, t)| {
                                *t == "(" && {
                                    let q = close + o;
                                    self.toks[rest[q]].kind == TokKind::Ident
                                        && !NOT_A_NAME.contains(&texts[q])
                                        && !matches!(
                                            texts[q],
                                            "const" | "noexcept" | "throw" | "override" | "final"
                                        )
                                }
                            });
                        let is_macro_prefix = p
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                            && later_call
                            && !matches!(after, Some(":") | Some("->") | None);
                        if is_macro_prefix {
                            j = close + 1;
                            continue;
                        }
                        open = Some(j);
                        name = p.to_string();
                        name_pos = j - 1;
                        // Destructor
                        if name_pos > 0 && texts[name_pos - 1] == "~" {
                            name = format!("~{name}");
                            name_pos -= 1;
                        }
                        break;
                    }
                    Some((p, TokKind::Ident)) if NOT_A_NAME.contains(&p) => {
                        if matches!(
                            p,
                            "if" | "for"
                                | "while"
                                | "switch"
                                | "return"
                                | "case"
                                | "do"
                                | "else"
                                | "goto"
                        ) {
                            return None;
                        }
                        j = matching_in(self.toks, rest, j, "(", ")") + 1;
                        continue;
                    }
                    _ => return None,
                }
            }
            j += 1;
        }
        let open = open?;
        let close = matching_in(self.toks, rest, open, "(", ")");
        if close >= rest.len() {
            return None;
        }
        // Qualification written on the declarator: `A::B::name`.
        let mut qual_parts = Vec::new();
        let mut q = name_pos;
        while q >= 2 && texts[q - 1] == "::" {
            let mut part_pos = q - 2;
            // Skip template args on the qualifier: `X<T>::f`.
            if texts[part_pos] == ">" {
                let mut depth = 0i32;
                let mut k = part_pos as isize;
                while k >= 0 {
                    match texts[k as usize] {
                        ">" => depth += 1,
                        "<" => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k -= 1;
                }
                if k <= 0 {
                    break;
                }
                part_pos = (k - 1) as usize;
            }
            if self.toks[rest[part_pos]].kind != TokKind::Ident {
                break;
            }
            qual_parts.insert(0, texts[part_pos].to_string());
            q = part_pos;
        }
        // A bare call statement at namespace scope (`FOO(x);`) has nothing before the name;
        // keep it (macro-declared entities) only when it has a body.
        let before = &texts[..q];
        let params_idx = &rest[open + 1..close];
        let params = join_tokens(self.toks, params_idx);
        let trailing_idx = &rest[close + 1..];
        let trailing = trailing_text(self.toks, trailing_idx);
        let is_virtual = before.contains(&"virtual") || trailing.contains("override");
        let mut scope = self.scope_path();
        if !qual_parts.is_empty() {
            if !scope.is_empty() {
                scope.push_str("::");
            }
            scope.push_str(&qual_parts.join("::"));
        }
        let name_line = self.toks[rest[name_pos]].line;
        let bare = before.iter().all(|t| {
            matches!(
                *t,
                "inline" | "constexpr" | "explicit" | "static" | "virtual" | "friend"
            )
        }) && qual_parts.is_empty();
        Some(Entity {
            bare,
            kind: EntityKind::Function,
            name,
            scope,
            param_types: param_types(&params),
            params,
            trailing,
            template,
            spec_args: None,
            bases: Vec::new(),
            is_virtual,
            is_def: false,
            line: start_line.unwrap_or(name_line),
            end_line: name_line,
            name_line,
            body: None,
        })
    }
}

fn normalise_operator(name: &str) -> String {
    let rest = name.trim_start_matches("operator").trim();
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        format!("operator {rest}")
    } else {
        format!("operator{}", rest.replace(' ', ""))
    }
}

/// `template <…>` headers at the start of a statement (possibly several), and the rest.
fn split_template_header<'s>(toks: &[Tok], stmt: &'s [usize]) -> (Option<String>, &'s [usize]) {
    let mut j = 0;
    let mut header = None;
    while j < stmt.len() && toks[stmt[j]].text == "template" {
        if j + 1 < stmt.len() && toks[stmt[j + 1]].text == "<" {
            let close = matching_angle(toks, stmt, j + 1);
            header = Some(join_tokens(toks, &stmt[j + 1..=close.min(stmt.len() - 1)]));
            j = close + 1;
        } else {
            break;
        }
    }
    (header, &stmt[j.min(stmt.len())..])
}

/// Index (into `stmt`) of the `:` that starts a constructor initializer list, if any.
fn ctor_init_colon(toks: &[Tok], stmt: &[usize]) -> Option<usize> {
    let mut depth = 0i32;
    let mut seen_params = false;
    for (pos, &k) in stmt.iter().enumerate() {
        match toks[k].text.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    seen_params = true;
                }
            }
            ":" if depth == 0 && seen_params => return Some(k),
            _ => {}
        }
        let _ = pos;
    }
    None
}

/// Index of the token closing the bracket opened at `open` (in the full token list).
fn matching(toks: &[Tok], open: usize, left: &str, right: &str) -> usize {
    let mut depth = 0i32;
    let mut k = open;
    while k < toks.len() {
        if toks[k].kind == TokKind::Punct {
            if toks[k].text == left {
                depth += 1;
            } else if toks[k].text == right {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
        }
        k += 1;
    }
    toks.len().saturating_sub(1)
}

/// Like [`matching`] but over positions of a statement slice; returns a position in `stmt`.
fn matching_in(toks: &[Tok], stmt: &[usize], open: usize, left: &str, right: &str) -> usize {
    let mut depth = 0i32;
    let mut k = open;
    while k < stmt.len() {
        let t = &toks[stmt[k]];
        if t.kind == TokKind::Punct {
            if t.text == left {
                depth += 1;
            } else if t.text == right {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
        }
        k += 1;
    }
    stmt.len()
}

/// Closing `>` for a `<` at `open` (statement positions), respecting parentheses.
fn matching_angle(toks: &[Tok], stmt: &[usize], open: usize) -> usize {
    let mut angle = 0i32;
    let mut paren = 0i32;
    let mut k = open;
    while k < stmt.len() {
        let t = &toks[stmt[k]];
        match t.text.as_str() {
            "(" | "[" => paren += 1,
            ")" | "]" => paren -= 1,
            "<" if paren == 0 => angle += 1,
            ">" if paren == 0 => {
                angle -= 1;
                if angle == 0 {
                    return k;
                }
            }
            ";" | "{" | "}" if paren == 0 => return k.saturating_sub(1),
            _ => {}
        }
        k += 1;
    }
    stmt.len().saturating_sub(1)
}

/// Join tokens with C++-ish spacing: a space only between two word-like tokens.
pub(crate) fn join_tokens(toks: &[Tok], idx: &[usize]) -> String {
    let mut out = String::new();
    let mut prev_word = false;
    for &k in idx {
        let Some(t) = toks.get(k) else { continue };
        if matches!(t.kind, TokKind::If | TokKind::Else | TokKind::Endif) {
            continue;
        }
        let word = matches!(t.kind, TokKind::Ident | TokKind::Number);
        let after_type_punct = out.ends_with('>') || out.ends_with('*') || out.ends_with('&');
        if word && (prev_word || after_type_punct) {
            out.push(' ');
        } else if t.text == "," {
            out.push_str(", ");
            prev_word = false;
            continue;
        }
        out.push_str(&t.text);
        prev_word = word;
    }
    out
}

fn trailing_text(toks: &[Tok], idx: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut k = 0;
    while k < idx.len() {
        let t = &toks[idx[k]];
        match t.text.as_str() {
            "const" | "volatile" | "&" | "&&" | "override" | "final" | "noexcept" => {
                parts.push(t.text.clone());
                // noexcept(expr)
                if t.text == "noexcept" && idx.get(k + 1).is_some_and(|&n| toks[n].text == "(") {
                    let close = matching_in(toks, idx, k + 1, "(", ")");
                    k = close + 1;
                    continue;
                }
            }
            "=" => {
                // = 0 / = default / = delete
                if let Some(&n) = idx.get(k + 1) {
                    parts.push(format!("= {}", toks[n].text));
                }
                break;
            }
            ":" | "->" => break,
            _ => {}
        }
        k += 1;
    }
    // `&` `&` from a split `&&` never happens (tokenizer emits single chars): join pairs.
    let joined = parts.join(" ");
    joined.replace("& &", "&&")
}

/// Split a parameter list on top-level commas and reduce each parameter to its type.
pub(crate) fn param_types(params: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in params.chars() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(param_type(&current));
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(param_type(&current));
    }
    if out.len() == 1 && (out[0] == "void" || out[0].is_empty()) {
        return Vec::new();
    }
    out
}

fn param_type(param: &str) -> String {
    // Drop a default argument.
    let mut depth = 0i32;
    let mut cut = param.len();
    for (pos, c) in param.char_indices() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            '=' if depth == 0 => {
                cut = pos;
                break;
            }
            _ => {}
        }
    }
    let text = param[..cut].trim();
    // Drop the parameter name: a trailing plain identifier (optionally with `[…]`) after at
    // least one type word that is not itself a bare specifier (`struct tm`, `const T`).
    let words: Vec<&str> = text.split(' ').collect();
    const SPECIFIERS: &[&str] = &[
        "struct", "class", "enum", "union", "typename", "const", "volatile", "unsigned", "signed",
        "long", "short",
    ];
    const BUILTIN: &[&str] = &[
        "int", "char", "long", "short", "unsigned", "signed", "double", "float", "bool", "void",
        "const", "volatile", "auto", "wchar_t", "char8_t", "char16_t", "char32_t", "size_t",
    ];
    let trimmed = if words.len() >= 2 {
        let last = words[words.len() - 1];
        let ident = last.split('[').next().unwrap_or(last);
        let is_name = !ident.is_empty()
            && ident
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            && ident.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !BUILTIN.contains(&ident)
            && !SPECIFIERS.contains(&words[words.len() - 2]);
        if is_name {
            words[..words.len() - 1].join(" ")
        } else {
            text.to_string()
        }
    } else {
        text.to_string()
    };
    trimmed
        .replace(" &", "&")
        .replace(" *", "*")
        .trim()
        .to_string()
}

fn looks_like_param_list(params: &str) -> bool {
    let trimmed = params.trim();
    if trimmed.is_empty() || trimmed == "void" {
        return true;
    }
    // Literals outside default arguments mean a call, not a declaration.
    for part in trimmed.split(',') {
        let head = part.split('=').next().unwrap_or("").trim();
        if head.is_empty() {
            return false;
        }
        let first = head.chars().next().unwrap_or(' ');
        if first == '"' || first == '\'' || first.is_ascii_digit() || first == '-' || first == '&' {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// switch / case
// ---------------------------------------------------------------------------------------------

fn find_switches(toks: &[Tok], entities: &[Entity]) -> Vec<Switch> {
    let mut out = Vec::new();
    for entity in entities {
        let Some((a, b)) = entity.body else { continue };
        let mut k = a;
        while k < b {
            if toks[k].kind == TokKind::Ident
                && toks[k].text == "switch"
                && toks.get(k + 1).is_some_and(|t| t.text == "(")
            {
                let close = matching(toks, k + 1, "(", ")");
                let expr = join_tokens(toks, &(k + 2..close).collect::<Vec<_>>());
                // Find the body brace.
                let mut brace = close + 1;
                while brace < b && toks[brace].text != "{" {
                    brace += 1;
                }
                if brace >= b {
                    k += 1;
                    continue;
                }
                let end = matching(toks, brace, "{", "}");
                let mut cases: Vec<(String, u32, u32)> = Vec::new();
                let mut depth = 0i32;
                let mut m = brace + 1;
                while m < end {
                    let t = &toks[m];
                    match t.text.as_str() {
                        "{" => depth += 1,
                        "}" => depth -= 1,
                        "switch" if t.kind == TokKind::Ident => {
                            // Nested switch: skip it entirely (it is found on its own).
                            if let Some(open) = (m..end).find(|&x| toks[x].text == "{") {
                                m = matching(toks, open, "{", "}") + 1;
                                continue;
                            }
                        }
                        "case" | "default" if t.kind == TokKind::Ident && depth <= 1 => {
                            let mut colon = m + 1;
                            let mut paren = 0i32;
                            while colon < end {
                                match toks[colon].text.as_str() {
                                    "(" => paren += 1,
                                    ")" => paren -= 1,
                                    ":" if paren == 0 => break,
                                    _ => {}
                                }
                                colon += 1;
                            }
                            let label = if t.text == "default" {
                                "default".to_string()
                            } else {
                                join_tokens(toks, &(m + 1..colon).collect::<Vec<_>>())
                            };
                            if let Some(last) = cases.last_mut() {
                                last.2 = t.line.saturating_sub(1).max(last.1);
                            }
                            cases.push((label, t.line, toks[end].line));
                            m = colon + 1;
                            continue;
                        }
                        _ => {}
                    }
                    m += 1;
                }
                out.push(Switch {
                    expr,
                    line: toks[k].line,
                    end_line: toks[end].line,
                    cases,
                });
                k = close + 1;
                continue;
            }
            k += 1;
        }
    }
    // Deduplicate (a switch inside a class member counted once).
    let mut seen = BTreeMap::new();
    out.retain(|s| seen.insert(s.line, ()).is_none());
    out
}

/// Word pieces of an identifier: `get_glyph_hAdvance` → [get, glyph, h, advance].
pub(crate) fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (pos, &c) in chars.iter().enumerate() {
        if c == '_' || c == '~' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current).to_ascii_lowercase());
            }
            continue;
        }
        let boundary = c.is_uppercase()
            && pos > 0
            && (chars[pos - 1].is_lowercase()
                || chars.get(pos + 1).is_some_and(|n| n.is_lowercase())
                    && chars[pos - 1].is_uppercase());
        if boundary && !current.is_empty() {
            words.push(std::mem::take(&mut current).to_ascii_lowercase());
        }
        current.push(c);
    }
    if !current.is_empty() {
        words.push(current.to_ascii_lowercase());
    }
    words
}

/// Jaccard similarity of token 3-shingles (0..=1).
pub(crate) fn shingle_similarity(a: &[&str], b: &[&str]) -> f64 {
    use std::collections::HashSet;
    if a.len() < 3 || b.len() < 3 {
        return if a == b && !a.is_empty() { 1.0 } else { 0.0 };
    }
    let sa: HashSet<(&str, &str, &str)> = a.windows(3).map(|w| (w[0], w[1], w[2])).collect();
    let sb: HashSet<(&str, &str, &str)> = b.windows(3).map(|w| (w[0], w[1], w[2])).collect();
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn names(scan: &FileScan) -> Vec<String> {
        scan.entities.iter().map(Entity::signature).collect()
    }

    #[test]
    fn finds_functions_classes_and_members() {
        let src = r#"
namespace lib {
namespace detail { int helper(int x); }
template <typename T>
class Widget : public Base<T>, private Other {
 public:
  size_t size() const { return n_; }
  size_t size() && noexcept;
  virtual void draw(Canvas& c) override;
  Widget(int a) : n_{a}, m_(a) { init(); }
  ~Widget();
 private:
  size_t n_;
};
inline std::tm gmtime(std::chrono::time_point<std::chrono::system_clock> tp) {
  return gmtime(to_time_t(tp));
}
auto Widget<int>::render(int x, const char* y = "a") -> void { if (x) { y++; } }
struct tm* localtime_r(const time_t* t, struct tm* out);
}
"#;
        let scan = scan(src);
        let sigs = names(&scan);
        assert!(
            sigs.contains(&"lib::detail::helper(int x)".to_string()),
            "{sigs:#?}"
        );
        assert!(sigs.contains(&"class lib::Widget".to_string()), "{sigs:#?}");
        assert!(
            sigs.contains(&"lib::Widget::size() const".to_string()),
            "{sigs:#?}"
        );
        assert!(
            sigs.contains(&"lib::Widget::size() && noexcept".to_string()),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter().any(|s| s.starts_with("lib::Widget::draw(")),
            "{sigs:#?}"
        );
        assert!(
            sigs.contains(&"lib::Widget::Widget(int a)".to_string()),
            "{sigs:#?}"
        );
        assert!(
            sigs.contains(&"lib::Widget::~Widget()".to_string()),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter()
                .any(|s| s.starts_with("lib::gmtime(std::chrono::time_point")),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter().any(|s| s.starts_with("lib::Widget::render(")),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter().any(|s| s.starts_with("lib::localtime_r(")),
            "{sigs:#?}"
        );
        let widget = scan
            .entities
            .iter()
            .find(|e| e.name == "Widget" && e.kind == EntityKind::Class)
            .unwrap();
        assert_eq!(widget.bases, vec!["Base".to_string(), "Other".to_string()]);
        let gm = scan.entities.iter().find(|e| e.name == "gmtime").unwrap();
        assert_eq!(gm.line, 15);
        assert_eq!(gm.end_line, 17);
        let draw = scan.entities.iter().find(|e| e.name == "draw").unwrap();
        assert!(draw.is_virtual);
        let render = scan.entities.iter().find(|e| e.name == "render").unwrap();
        assert_eq!(
            render.param_types,
            vec!["int".to_string(), "const char*".to_string()]
        );
    }

    #[test]
    fn operators_specialisations_and_macro_prefixes() {
        let src = r#"
template <> struct formatter<std::queue<int>, char> : formatter<int> {
  auto format(const std::queue<int>& q, out_context& ctx) const -> decltype(ctx.out());
};
template <typename T, typename Char>
struct formatter<T, Char, enable_if_t<is_sequence<T, Char>::value>> {};
std::ostream& operator<<(std::ostream& out, const element& e) { return out; }
bool operator==(const A& a, const A& b);
LIB_API void vlog(string_view fmt, arg_list args) {}
TEST(Chrono, Gmtime) { EXPECT_EQ(1, 1); }
"#;
        let scan = scan(src);
        let sigs = names(&scan);
        assert!(
            sigs.contains(&"class formatter<std::queue<int>, char>".to_string()),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter()
                .any(|s| s.starts_with("class formatter<T, Char, enable_if_t<")),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter()
                .any(|s| s.starts_with("operator<<(std::ostream")),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter().any(|s| s.starts_with("operator==(")),
            "{sigs:#?}"
        );
        assert!(sigs.iter().any(|s| s.starts_with("vlog(")), "{sigs:#?}");
        assert!(
            sigs.iter().any(|s| s.starts_with("TEST(Chrono, Gmtime)")),
            "{sigs:#?}"
        );
        assert!(
            sigs.iter().any(|s| s.starts_with("formatter::format(")),
            "{sigs:#?}"
        );
    }

    #[test]
    fn if_else_branches_are_both_scanned() {
        let src = "#ifndef GUARD_H\n#define GUARD_H\n#if HAS_EXC\nbool is_null() const {\n  return x;\n}\n#else\nresult<bool> is_null() const {\n  return y;\n}\n#endif\nint after() { return 1; }\n#endif\n";
        let scan = scan(src);
        let nulls: Vec<&Entity> = scan
            .entities
            .iter()
            .filter(|e| e.name == "is_null")
            .collect();
        assert_eq!(nulls.len(), 2, "{:#?}", scan.entities);
        assert!(scan.entities.iter().any(|e| e.name == "after"));
        let at4 = branches_at(&scan.branches, 4);
        assert_eq!(at4.len(), 1, "{:#?}", scan.branches);
        assert_eq!(at4[0].condition, "if HAS_EXC");
        let at8 = branches_at(&scan.branches, 8);
        assert_eq!(at8[0].condition, "else");
        assert_eq!(at8[0].group, at4[0].group);
        // The include guard is not a configuration branch.
        assert!(branches_at(&scan.branches, 12).is_empty());
    }

    #[test]
    fn switch_cases_with_ranges() {
        let src = r#"
void parse(char c) {
  switch (c) {
  case 'a':
  case 'b':
    handle_ab();
    break;
  case '-': {
    pad = none;
    break;
  }
  default:
    fail();
  }
}
"#;
        let scan = scan(src);
        assert_eq!(scan.switches.len(), 1);
        let sw = &scan.switches[0];
        assert_eq!(sw.expr, "c");
        let labels: Vec<&str> = sw.cases.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(labels, vec!["'a'", "'b'", "'-'", "default"]);
        assert_eq!(sw.cases[2].1, 8);
    }

    #[test]
    fn words_and_similarity() {
        assert_eq!(
            name_words("hb_ft_get_glyph_h_advances"),
            vec!["hb", "ft", "get", "glyph", "h", "advances"]
        );
        assert_eq!(
            name_words("getGlyphHAdvance"),
            vec!["get", "glyph", "h", "advance"]
        );
        assert_eq!(
            name_words("parseHTTPHeader"),
            vec!["parse", "http", "header"]
        );
        let a = ["a", "b", "c", "d", "e"];
        assert!((shingle_similarity(&a, &a) - 1.0).abs() < 1e-9);
        assert!(shingle_similarity(&a, &["x", "y", "z"]) < 0.01);
    }

    #[test]
    fn raw_strings_and_comments_do_not_confuse_braces() {
        let src = "const char* s = R\"x({ not a brace )x\";\n/* { */ int f() { return '}'; }\n// {\nint g() { return 0; }\n";
        let scan = scan(src);
        let sigs = names(&scan);
        assert!(sigs.contains(&"f()".to_string()), "{sigs:#?}");
        assert!(sigs.contains(&"g()".to_string()), "{sigs:#?}");
    }
}
