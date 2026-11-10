//! A small, dependency-free parser for GDB/MI3 output records.
//!
//! GDB's machine interface emits one record per line. We only need enough of the
//! grammar to (a) separate the human-readable *console stream* records (`~"…"`)
//! from machine records, and (b) pull a few fields out of async stop records
//! (`*stopped,reason=…,frame={…}`) and error results (`^error,msg="…"`). Result
//! tuples/lists are parsed into a [`serde_json::Value`] so callers can read
//! fields by name without bespoke extraction code.

use serde_json::Map;
use serde_json::Value;

/// The three GDB/MI stream-record flavors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    /// `~` — console output: exactly what a user sees typing the command.
    Console,
    /// `@` — target (inferior) output.
    Target,
    /// `&` — GDB's own log/echo; not shown to the model.
    Log,
}

/// The three GDB/MI out-of-band async-record flavors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsyncKind {
    /// `*` — execution state changed (e.g. `*stopped`, `*running`).
    Exec,
    /// `+` — ongoing-operation status.
    Status,
    /// `=` — notification (library loaded, thread created, …).
    Notify,
}

/// One parsed MI record.
#[derive(Debug, Clone)]
pub(crate) enum MiRecord {
    /// `^class[,results]` — the result of a command, tagged with its token.
    Result {
        token: Option<u64>,
        class: String,
        payload: Value,
    },
    /// `*/+/= class[,results]` — an out-of-band async record.
    Async {
        kind: AsyncKind,
        class: String,
        payload: Value,
    },
    /// `~/@/& "…"` — a stream record carrying free text.
    Stream { kind: StreamKind, text: String },
    /// The `(gdb)` terminator line.
    Prompt,
    /// Anything we don't recognize (e.g. raw inferior output).
    Other(String),
}

/// Parse a single line of MI output into a [`MiRecord`].
pub(crate) fn parse_mi_line(raw: &str) -> MiRecord {
    let line = raw.trim_end_matches(['\r', '\n']);
    if line.trim_end() == "(gdb)" {
        return MiRecord::Prompt;
    }

    // An optional leading token is a run of ASCII digits. Because they are
    // ASCII, the char count equals the byte offset, so slicing is safe.
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let token = if digits > 0 {
        line[..digits].parse::<u64>().ok()
    } else {
        None
    };
    let rest = &line[digits..];
    let Some(kind) = rest.chars().next() else {
        return MiRecord::Other(line.to_string());
    };
    let body = &rest[kind.len_utf8()..];

    match kind {
        '~' => stream(StreamKind::Console, body),
        '@' => stream(StreamKind::Target, body),
        '&' => stream(StreamKind::Log, body),
        '^' => {
            let (class, payload) = split_class(body);
            MiRecord::Result {
                token,
                class,
                payload,
            }
        }
        '*' => async_record(AsyncKind::Exec, body),
        '+' => async_record(AsyncKind::Status, body),
        '=' => async_record(AsyncKind::Notify, body),
        _ => MiRecord::Other(line.to_string()),
    }
}

fn stream(kind: StreamKind, body: &str) -> MiRecord {
    MiRecord::Stream {
        kind,
        text: parse_stream_cstring(body),
    }
}

fn async_record(kind: AsyncKind, body: &str) -> MiRecord {
    let (class, payload) = split_class(body);
    MiRecord::Async {
        kind,
        class,
        payload,
    }
}

/// Split `class[,results]` into the class string and a JSON object of results.
fn split_class(body: &str) -> (String, Value) {
    let class: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let rest = &body[class.len()..];
    let payload = match rest.strip_prefix(',') {
        Some(results) => parse_results_object(results),
        None => Value::Object(Map::new()),
    };
    (class, payload)
}

/// Decode a whole stream-record body (`"…"`) into its text.
fn parse_stream_cstring(body: &str) -> String {
    let trimmed = body.trim();
    let inner = trimmed
        .strip_prefix('"')
        .map(|s| s.strip_suffix('"').unwrap_or(s))
        .unwrap_or(trimmed);
    unescape_c(inner)
}

/// Apply C-string unescaping to already-unquoted contents.
fn unescape_c(s: &str) -> String {
    let mut chars = s.chars().peekable();
    let mut out = Vec::with_capacity(s.len());
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(esc) = chars.next() {
                push_escaped(&mut out, esc, || chars.next_if(|next| next.is_digit(8)));
            }
        } else {
            push_char(&mut out, c);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Append the bytes a `\<esc>` sequence stands for. gdb writes every byte
/// outside printable ASCII as an octal escape — `café` is `caf\303\251` —
/// so a string is decoded to BYTES and only then read as UTF-8. Pushing each
/// escape's digits as text is how `café-中文` reached the model as
/// `caf303251-344270255346226207`. `next_octal` yields the following octal
/// digit, if any, consuming it.
fn push_escaped(out: &mut Vec<u8>, esc: char, mut next_octal: impl FnMut() -> Option<char>) {
    match esc {
        '0'..='7' => {
            let mut value = esc as u32 - '0' as u32;
            for _ in 0..2 {
                match next_octal() {
                    Some(digit) => value = value * 8 + (digit as u32 - '0' as u32),
                    None => break,
                }
            }
            // Three octal digits can exceed a byte (\777); keep the low byte
            // rather than panic on input gdb would never send.
            out.push(value as u8);
        }
        'n' => out.push(b'\n'),
        't' => out.push(b'\t'),
        'r' => out.push(b'\r'),
        'a' => out.push(0x07),
        'b' => out.push(0x08),
        'f' => out.push(0x0c),
        'v' => out.push(0x0b),
        'e' => out.push(0x1b),
        other => push_char(out, other),
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// Parse a top-level `var=value,var=value,…` list into a JSON object.
fn parse_results_object(s: &str) -> Value {
    let mut cur = Cursor::new(s);
    let mut map = Map::new();
    while cur.looks_like_result() {
        let (k, v) = cur.parse_result();
        map.insert(k, v);
        if !cur.eat(',') {
            break;
        }
    }
    Value::Object(map)
}

/// A char cursor over a single MI results body.
struct Cursor {
    chars: Vec<char>,
    pos: usize,
}

impl Cursor {
    fn new(s: &str) -> Self {
        Self {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Consume `c` if it is next; report whether it was.
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Does the cursor sit at the start of a `name=` result?
    fn looks_like_result(&self) -> bool {
        let mut j = self.pos;
        while matches!(self.chars.get(j), Some(c) if is_name_char(*c)) {
            j += 1;
        }
        j > self.pos && self.chars.get(j) == Some(&'=')
    }

    fn parse_result(&mut self) -> (String, Value) {
        let name = self.parse_name();
        if self.eat('=') {
            (name, self.parse_value())
        } else {
            (name, Value::Null)
        }
    }

    fn parse_name(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if is_name_char(c) {
                name.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        name
    }

    fn parse_value(&mut self) -> Value {
        match self.peek() {
            Some('"') => Value::String(self.parse_cstring()),
            Some('{') => self.parse_tuple(),
            Some('[') => self.parse_list(),
            _ => Value::String(self.parse_bare()),
        }
    }

    fn parse_cstring(&mut self) -> String {
        self.bump(); // opening quote
        let mut out = Vec::new();
        while let Some(c) = self.bump() {
            match c {
                '\\' => {
                    if let Some(esc) = self.bump() {
                        push_escaped(&mut out, esc, || {
                            let digit = self.peek().filter(|next| next.is_digit(8))?;
                            self.pos += 1;
                            Some(digit)
                        });
                    }
                }
                '"' => break,
                other => push_char(&mut out, other),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn parse_tuple(&mut self) -> Value {
        self.bump(); // {
        let mut map = Map::new();
        if self.eat('}') {
            return Value::Object(map);
        }
        loop {
            let (k, v) = self.parse_result();
            map.insert(k, v);
            if self.eat(',') {
                continue;
            }
            self.eat('}');
            break;
        }
        Value::Object(map)
    }

    fn parse_list(&mut self) -> Value {
        self.bump(); // [
        let mut arr = Vec::new();
        if self.eat(']') {
            return Value::Array(arr);
        }
        loop {
            if self.looks_like_result() {
                let (k, v) = self.parse_result();
                let mut m = Map::new();
                m.insert(k, v);
                arr.push(Value::Object(m));
            } else {
                arr.push(self.parse_value());
            }
            if self.eat(',') {
                continue;
            }
            self.eat(']');
            break;
        }
        Value::Array(arr)
    }

    /// A bare (unquoted) value runs until the next structural delimiter.
    fn parse_bare(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if matches!(c, ',' | '}' | ']') {
                break;
            }
            out.push(c);
            self.pos += 1;
        }
        out
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn octal_escapes_decode_to_utf8() {
        // Verbatim from gdb 13: every non-ASCII byte arrives as `\NNN`, and a
        // multi-byte character is several of them.
        let line =
            r#"~"3\tstd::string s = \"caf\303\251-\344\270\255\346\226\207-\360\237\232\200\";\n""#;
        match parse_mi_line(line) {
            MiRecord::Stream { text, .. } => {
                assert_eq!(text, "3\tstd::string s = \"café-中文-🚀\";\n");
            }
            other => panic!("{other:?}"),
        }
        // The same inside a result tuple, and the escapes around it.
        let line = r#"^done,value="na\303\257ve \"q\" \\ \000end""#;
        match parse_mi_line(line) {
            MiRecord::Result { payload, .. } => {
                assert_eq!(payload["value"], "naïve \"q\" \\ \0end");
            }
            other => panic!("{other:?}"),
        }
        // gdb's own escape of a byte it would not print stays readable text.
        assert_eq!(unescape_c(r#"H\\213L"#), r"H\213L");
    }

    #[test]
    fn parses_bare_done_result() {
        match parse_mi_line("^done") {
            MiRecord::Result {
                token,
                class,
                payload,
            } => {
                assert_eq!(token, None);
                assert_eq!(class, "done");
                assert_eq!(payload, Value::Object(Map::new()));
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn parses_token_prefix() {
        match parse_mi_line("42^done") {
            MiRecord::Result { token, .. } => assert_eq!(token, Some(42)),
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_message_with_embedded_quotes() {
        match parse_mi_line(r#"^error,msg="No symbol \"x\" in current context.""#) {
            MiRecord::Result { class, payload, .. } => {
                assert_eq!(class, "error");
                assert_eq!(
                    payload.get("msg").and_then(Value::as_str),
                    Some(r#"No symbol "x" in current context."#)
                );
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn parses_stopped_with_nested_frame() {
        let line = r#"*stopped,reason="breakpoint-hit",disp="keep",bkptno="1",frame={addr="0x1149",func="main",args=[],file="demo.c",fullname="/tmp/demo.c",line="42"},thread-id="1",stopped-threads="all""#;
        match parse_mi_line(line) {
            MiRecord::Async {
                kind,
                class,
                payload,
            } => {
                assert_eq!(kind, AsyncKind::Exec);
                assert_eq!(class, "stopped");
                assert_eq!(
                    payload.get("reason").and_then(Value::as_str),
                    Some("breakpoint-hit")
                );
                let frame = payload.get("frame").expect("frame present");
                assert_eq!(frame.get("func").and_then(Value::as_str), Some("main"));
                assert_eq!(frame.get("file").and_then(Value::as_str), Some("demo.c"));
                assert_eq!(frame.get("line").and_then(Value::as_str), Some("42"));
            }
            other => panic!("expected async, got {other:?}"),
        }
    }

    #[test]
    fn console_stream_is_unescaped() {
        match parse_mi_line(r#"~"Breakpoint 1 at 0x1149: file demo.c, line 42.\n""#) {
            MiRecord::Stream { kind, text } => {
                assert_eq!(kind, StreamKind::Console);
                assert_eq!(text, "Breakpoint 1 at 0x1149: file demo.c, line 42.\n");
            }
            other => panic!("expected stream, got {other:?}"),
        }
    }

    #[test]
    fn log_stream_is_tagged() {
        match parse_mi_line(r#"&"some log\n""#) {
            MiRecord::Stream { kind, .. } => assert_eq!(kind, StreamKind::Log),
            other => panic!("expected stream, got {other:?}"),
        }
    }

    #[test]
    fn recognizes_prompt_and_notify() {
        assert!(matches!(parse_mi_line("(gdb) "), MiRecord::Prompt));
        match parse_mi_line(r#"=thread-group-added,id="i1""#) {
            MiRecord::Async { kind, class, .. } => {
                assert_eq!(kind, AsyncKind::Notify);
                assert_eq!(class, "thread-group-added");
            }
            other => panic!("expected async notify, got {other:?}"),
        }
    }

    #[test]
    fn running_result_class() {
        match parse_mi_line("^running") {
            MiRecord::Result { class, .. } => assert_eq!(class, "running"),
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn list_of_frames_parses() {
        let line = r#"^done,stack=[frame={level="0",func="a"},frame={level="1",func="b"}]"#;
        match parse_mi_line(line) {
            MiRecord::Result { payload, .. } => {
                let stack = payload
                    .get("stack")
                    .and_then(Value::as_array)
                    .expect("array");
                assert_eq!(stack.len(), 2);
            }
            other => panic!("expected result, got {other:?}"),
        }
    }
}
