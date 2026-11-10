//! Copy guard over every ByteAsk-authored string in the TUI.
//!
//! The copy rules are product rules, not a style preference: the brand words a user reads are the
//! whole point of the fork, and a single emoji or exclamation mark is the
//! difference between the utility voice the design specifies and the tone it forbids. Reviews
//! miss these one at a time; this test reads the sources at test time and does not.
//!
//! **What it reads.** The ByteAsk-authored files only — `tui/src/byteask_*.rs`,
//! `tui/src/chatwidget/byteask_*.rs`, `tui/src/byteask_settings/*.rs`, plus
//! `tui/src/bottom_pane/info_pane_view.rs` and `tui/src/onboarding/signin.rs`. Files that do
//! not exist are skipped, so the guard is stable while those two are being written. Upstream
//! files are deliberately out of scope: they are rebranded by
//! the rebranding script at release time, and duplicating that here would report a
//! leak the release already closes.
//!
//! **What it skips, and why.** Everything from the first `#[cfg(test)]` line to the end of the
//! file — test fixtures are not copy, and a test that asserts on a bad string must be able to
//! spell it. Comments are skipped by the lexer for the same reason: a comment explaining a
//! brand rule has to be able to name the brand.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

/// A literal that breaks a rule but is allowed to stay, with the reason it is allowed.
///
/// Matching is on the literal's text, not on a file and line, so an entry survives the code
/// around it moving. An entry that no longer matches anything is *not* an error: these files
/// are rewritten by the surface work this guard was written alongside, and a stale entry means
/// the debt was paid, which must never be reported as a regression.
struct Allowed {
    literal: &'static str,
    reason: &'static str,
}

const ALLOWLIST: &[Allowed] = &[
    Allowed {
        literal: "OpenAI",
        reason: "byteask_settings::store::KeyProvider::label -- the API-keys row the user \
                 types their own OpenAI key into. Rule 6 names this exact exception: a \
                 provider may be named where the user supplies that provider's key. Renaming \
                 it would leave the user guessing which key the row wants.",
    },
    Allowed {
        literal: "Use my Claude Code",
        reason: "byteask_settings::views -- the row that runs the user's OWN Claude Code \
                 install on their own subscription. The product being launched has to be \
                 named or the row is unusable, and Anthropic's own wording (\"runs Claude \
                 Code\") is what makes this delegation permissible. Renaming it would \
                 describe a different product from the one that starts.",
    },
    Allowed {
        literal: "Run your own Claude Code on your subscription",
        reason: "byteask_settings::views -- the subtitle of the row above. Same exception, \
                 and it carries the two facts that keep the row honest: it is the user's \
                 OWN install, on the user's OWN subscription, not ByteAsk reselling it.",
    },
];

/// The rules, in the order they are reported. Each is a product rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// The fork's own name leaking through. Real model ids are allowed (`GPT-5.3 Codex`).
    Codex,
    /// A provider named outside the "the user supplies that provider's key" exception.
    OpenAi,
    Fireworks,
    Vertex,
    /// A competitor named in anything the user reads.
    Competitor,
    /// A payment provider's own domain: the user must only ever see a ByteAsk URL.
    PaymentDomain,
    /// No emoji in ByteAsk-authored strings; the vocabulary is `✓ ✗ › ▌ █ ░ ·`.
    Emoji,
    /// Utility voice: no exclamation marks.
    Exclamation,
    /// A slug where a display name belongs -- prose says `Auto`, never `auto`.
    AutoSlug,
}

impl Rule {
    fn describe(self) -> &'static str {
        match self {
            Rule::Codex => "names Codex (real model ids such as `GPT-5.3 Codex` are allowed)",
            Rule::OpenAi => {
                "names OpenAI outside the provider-key exception (allowed when the literal \
                 also mentions a key, or an OpenAI-compatible endpoint the user runs)"
            }
            Rule::Fireworks => "names Fireworks, a provider the user never chose",
            Rule::Vertex => "names Vertex, a provider the user never chose",
            Rule::Competitor => "names a competitor",
            Rule::PaymentDomain => "shows a payment provider's domain instead of a ByteAsk URL",
            Rule::Emoji => "contains an emoji; the glyph vocabulary is ✓ ✗ › ▌ █ ░ ·",
            Rule::Exclamation => "contains an exclamation mark",
            Rule::AutoSlug => "writes the slug `auto` in prose; the display name is `Auto`",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    line: usize,
    literal: String,
    rule: Rule,
}

// ---------------------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------------------

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `Codex` with a capital C is the brand; lowercase `codex` is a functional slug (a path, a
/// config key, a crate name) and is deliberately not policed -- the same split
/// the rebranding script makes, for the same reason: the blanket lowercase replace
/// once broke working slugs.
fn names_codex(literal: &str) -> bool {
    let chars: Vec<char> = literal.chars().collect();
    for start in 0..chars.len() {
        if !chars[start..].starts_with(&['C', 'o', 'd', 'e', 'x']) {
            continue;
        }
        let end = start + 5;
        if end < chars.len() && is_word_char(chars[end]) {
            continue; // part of a longer identifier, e.g. `CodexAuth`
        }
        // A real model id keeps its real name: `GPT-5.3 Codex` is a product, and
        // renaming it would invent one. Recognised by the `gpt<version><sep>` run in front.
        let prefix: String = chars[start.saturating_sub(16)..start]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        let looks_like_a_model_id = prefix.contains("gpt")
            && matches!(prefix.chars().next_back(), Some('-') | Some(' '));
        if !looks_like_a_model_id {
            return true;
        }
    }
    false
}

/// The copy rules allow a provider's name where the user supplies that provider's key, and where the
/// user points ByteAsk at their own server that speaks that provider's wire API.
fn names_openai(literal: &str) -> bool {
    if !literal.contains("OpenAI") {
        return false;
    }
    let lower = literal.to_ascii_lowercase();
    !(lower.contains("key") || lower.contains("-compatible"))
}

/// The ByteAsk glyph vocabulary plus the punctuation the panes use.
/// These share Unicode blocks with emoji, so they are named rather than ranged out.
const GLYPH_VOCABULARY: &[char] = &[
    '\u{2713}', // ✓
    '\u{2717}', // ✗
    '\u{203A}', // ›
    '\u{258C}', // ▌
    '\u{2588}', // █
    '\u{2591}', // ░
    '\u{2593}', // ▓
    '\u{00B7}', // ·
    '\u{2026}', // …
    '\u{2014}', // —
    '\u{2192}', // →
    '\u{00D7}', // ×
    '\u{280B}', // ⠋ braille spinner
];

/// The emoji this guard was written to catch, named so the intent survives a range edit.
const NAMED_EMOJI: &[char] = &['\u{1F389}', '\u{26A1}', '\u{1F512}', '\u{2728}'];

fn emoji_in(literal: &str) -> Option<char> {
    literal.chars().find(|c| {
        if GLYPH_VOCABULARY.contains(c) {
            return false;
        }
        if NAMED_EMOJI.contains(c) {
            return true;
        }
        let o = *c as u32;
        // Emoticons, pictographs, transport, supplemental and extended-A blocks; the
        // Miscellaneous Symbols and Dingbats blocks the older emoji live in; the
        // Miscellaneous Symbols and Arrows block (⭐); and the emoji-presentation and
        // keycap selectors.
        (0x1F000..=0x1FAFF).contains(&o)
            || (0x2600..=0x27BF).contains(&o)
            || (0x2B00..=0x2BFF).contains(&o)
            || o == 0xFE0F
            || o == 0x20E3
    })
}

/// `auto` is a slug; prose uses the display name `Auto`. Only checked in literals with a
/// space, because a bare `"auto"` is the wire value and must stay lowercase.
fn writes_the_auto_slug(literal: &str) -> bool {
    if !literal.contains(' ') {
        return false;
    }
    let chars: Vec<char> = literal.chars().collect();
    let boundary = |c: char| !(is_word_char(c) || c == '-' || c == '/' || c == '.');
    for start in 0..chars.len() {
        if !chars[start..].starts_with(&['a', 'u', 't', 'o']) {
            continue;
        }
        let before_ok = start == 0 || boundary(chars[start - 1]);
        let after_ok = start + 4 == chars.len() || boundary(chars[start + 4]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn broken_rule(literal: &str) -> Option<Rule> {
    if names_codex(literal) {
        return Some(Rule::Codex);
    }
    if names_openai(literal) {
        return Some(Rule::OpenAi);
    }
    if literal.contains("Fireworks") {
        return Some(Rule::Fireworks);
    }
    if literal.contains("Vertex") {
        return Some(Rule::Vertex);
    }
    if literal.contains("Claude Code") || literal.contains("Cursor") {
        return Some(Rule::Competitor);
    }
    if literal.contains("rzp.io") {
        return Some(Rule::PaymentDomain);
    }
    if emoji_in(literal).is_some() {
        return Some(Rule::Emoji);
    }
    // A one-character literal is a glyph or a marker, not a sentence, so it has no tone to
    // police -- a one-character literal is a glyph, not a sentence.
    if literal.contains('!') && literal.chars().count() > 1 {
        return Some(Rule::Exclamation);
    }
    if writes_the_auto_slug(literal) {
        return Some(Rule::AutoSlug);
    }
    None
}

// ---------------------------------------------------------------------------------------
// Reading the sources
// ---------------------------------------------------------------------------------------

/// Everything from the first `#[cfg(test)]` line onwards is test code, not copy.
fn strip_test_module(source: &str) -> &str {
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        if line.trim() == "#[cfg(test)]" {
            return &source[..offset];
        }
        offset += line.len();
    }
    source
}

/// Decode a `\u{XXXX}` escape starting at the backslash `at`, returning the character and the
/// index just past the closing brace.
fn decode_unicode_escape(chars: &[char], at: usize) -> Option<(char, usize)> {
    if chars.get(at + 1) != Some(&'u') || chars.get(at + 2) != Some(&'{') {
        return None;
    }
    let mut hex = String::new();
    let mut j = at + 3;
    while let Some(&c) = chars.get(j) {
        if c == '}' {
            let value = u32::from_str_radix(&hex, 16).ok()?;
            return char::from_u32(value).map(|c| (c, j + 1));
        }
        if !c.is_ascii_hexdigit() || hex.len() >= 6 {
            return None;
        }
        hex.push(c);
        j += 1;
    }
    None
}

/// Extract every string literal with its 1-based line number.
///
/// A hand-rolled lexer rather than a regex: comments must be skipped (they explain the rules
/// and so have to name the brands), and Rust's lifetimes (`'a`) look exactly like an unclosed
/// character literal to anything simpler.
fn string_literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        // Comments.
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 1usize;
            i += 2;
            while i < chars.len() && depth > 0 {
                if chars[i] == '\n' {
                    line += 1;
                } else if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 1;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 1;
                }
                i += 1;
            }
            continue;
        }
        // Raw strings: r"..", r#".."#, and the br/cr prefixed forms.
        if c == 'r' || ((c == 'b' || c == 'c') && chars.get(i + 1) == Some(&'r')) {
            let r_at = if c == 'r' { i } else { i + 1 };
            let mut hashes = 0usize;
            let mut j = r_at + 1;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) == Some(&'"') && (r_at == 0 || !is_word_char(chars[r_at - 1])) {
                let start_line = line;
                let mut lit = String::new();
                j += 1;
                'raw: while j < chars.len() {
                    if chars[j] == '"' {
                        let closed = (1..=hashes).all(|k| chars.get(j + k) == Some(&'#'));
                        if closed {
                            j += hashes + 1;
                            break 'raw;
                        }
                    }
                    if chars[j] == '\n' {
                        line += 1;
                    }
                    lit.push(chars[j]);
                    j += 1;
                }
                out.push((start_line, lit));
                i = j;
                continue;
            }
        }
        // Character literals -- distinguished from lifetimes by having a closing quote.
        if c == '\'' {
            let escaped = chars.get(i + 1) == Some(&'\\');
            let close = if escaped { None } else { chars.get(i + 2) };
            if close == Some(&'\'') {
                i += 3;
                continue;
            }
            if escaped {
                let mut j = i + 2;
                while j < chars.len() && chars[j] != '\'' && chars[j] != '\n' {
                    j += 1;
                }
                if chars.get(j) == Some(&'\'') {
                    i = j + 1;
                    continue;
                }
            }
            i += 1; // a lifetime
            continue;
        }
        // Ordinary strings, including the b"" / c"" prefixed forms.
        if c == '"' {
            let start_line = line;
            let mut lit = String::new();
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j] {
                    '\\' => {
                        // `\u{...}` is decoded, because it is the one escape that can smuggle
                        // a banned character past a text search -- `"\u{1F389} Pro claimed"`
                        // renders as an emoji and must be caught as one. Every other escape is
                        // kept verbatim; no rule reads through a `\n` or a `\"`.
                        if let Some(decoded) = decode_unicode_escape(&chars, j) {
                            lit.push(decoded.0);
                            j = decoded.1;
                            continue;
                        }
                        if let Some(next) = chars.get(j + 1) {
                            if *next == '\n' {
                                line += 1;
                            }
                            lit.push('\\');
                            lit.push(*next);
                        }
                        j += 2;
                    }
                    '"' => {
                        j += 1;
                        break;
                    }
                    ch => {
                        if ch == '\n' {
                            line += 1;
                        }
                        lit.push(ch);
                        j += 1;
                    }
                }
            }
            out.push((start_line, lit));
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

fn violations(source: &str) -> Vec<Finding> {
    let allowed: BTreeSet<&str> = ALLOWLIST.iter().map(|a| a.literal).collect();
    string_literals(strip_test_module(source))
        .into_iter()
        .filter(|(_, literal)| !allowed.contains(literal.as_str()))
        .filter_map(|(line, literal)| broken_rule(&literal).map(|rule| Finding { line, literal, rule }))
        .collect()
}

fn tui_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The `.rs` files in `dir` whose name starts with `prefix` (all of them when `None`).
/// A directory that does not exist yields nothing, so the guard is stable while the surface
/// work is still creating files.
fn rs_files_in(dir: &Path, prefix: Option<&str>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            // This file quotes every banned word as test data; guarding it would be a
            // guaranteed false positive.
            name != "byteask_copy_guard_tests.rs"
                && prefix.is_none_or(|prefix| name.starts_with(prefix))
        })
        .collect();
    found.sort();
    found
}

/// Every ByteAsk-authored source, resolved at test time. Directories are globbed rather than
/// listed so a file added by the surface work is guarded the moment it lands.
fn guarded_files() -> Vec<PathBuf> {
    let src = tui_src();
    let mut files = rs_files_in(&src, Some("byteask_"));
    files.extend(rs_files_in(&src.join("chatwidget"), Some("byteask_")));
    files.extend(rs_files_in(&src.join("byteask_settings"), None));

    for explicit in [
        src.join("bottom_pane").join("info_pane_view.rs"),
        src.join("onboarding").join("signin.rs"),
    ] {
        if explicit.is_file() {
            files.push(explicit);
        }
    }
    files
}

// ---------------------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------------------

#[test]
fn byteask_copy_follows_the_design_rules() {
    let files = guarded_files();
    assert!(
        !files.is_empty(),
        "the copy guard found no ByteAsk sources under {} -- the globs are wrong, and a guard \
         that reads nothing passes forever",
        tui_src().display()
    );

    let mut report = String::new();
    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for finding in violations(&source) {
            report.push_str(&format!(
                "\n{}:{}\n    {}\n    literal: {:?}\n",
                path.display(),
                finding.line,
                finding.rule.describe(),
                finding.literal,
            ));
        }
    }

    assert!(
        report.is_empty(),
        "ByteAsk copy breaks the copy rules this file checks.\n\
         Rewrite the line, or -- only for a deliberate exception -- add it to \
         ALLOWLIST in tui/src/byteask_copy_guard_tests.rs with the reason.\n{report}"
    );
}

/// Every allowlist entry has to say why, or the list becomes a place to hide copy.
#[test]
fn every_allowlist_entry_gives_a_reason() {
    for entry in ALLOWLIST {
        assert!(
            entry.reason.len() > 40,
            "allowlist entry {:?} needs a real reason, not {:?}",
            entry.literal,
            entry.reason
        );
        assert!(
            broken_rule(entry.literal).is_some(),
            "allowlist entry {:?} breaks no rule -- delete it rather than carrying it",
            entry.literal
        );
    }
}

// ---------------------------------------------------------------------------------------
// The guard's own tests: a copy guard that cannot fail is worth nothing.
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod guard_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn rules(source: &str) -> Vec<Rule> {
        violations(source).into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn each_rule_fires_on_a_planted_violation() {
        let cases: &[(&str, Rule)] = &[
            (r#"let s = "Codex is thinking";"#, Rule::Codex),
            (r#"let s = "Powered by OpenAI";"#, Rule::OpenAi),
            (r#"let s = "Served by Fireworks";"#, Rule::Fireworks),
            (r#"let s = "Routed through Vertex";"#, Rule::Vertex),
            (r#"let s = "Faster than Claude Code";"#, Rule::Competitor),
            (r#"let s = "Nicer than Cursor";"#, Rule::Competitor),
            (r#"let s = "Pay at https://rzp.io/i/x";"#, Rule::PaymentDomain),
            (r#"let s = "\u{1F389} Pro claimed";"#, Rule::Emoji),
            (r#"let s = "Signed in successfully!";"#, Rule::Exclamation),
            (r#"let s = "answered with auto instead";"#, Rule::AutoSlug),
        ];
        for (source, expected) in cases {
            assert_eq!(rules(source), vec![*expected], "source: {source}");
        }
    }

    #[test]
    fn an_escaped_emoji_is_still_an_emoji() {
        // The literal below is written with a `\u{...}` escape, which renders as 🎉 but reads
        // as plain ASCII in the source. Without decoding, this is a silent bypass.
        assert_eq!(rules(r#"let s = "\u{1F389} Pro claimed";"#), vec![Rule::Emoji]);
        // A backslash that is not a unicode escape is left alone.
        assert_eq!(rules(r#"let s = "line\nbreak";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "say \"hi\" now";"#), Vec::<Rule>::new());
    }

    #[test]
    fn emoji_rule_catches_the_named_ones() {
        for emoji in NAMED_EMOJI {
            let source = format!("let s = \"{emoji} something happened\";");
            assert_eq!(rules(&source), vec![Rule::Emoji], "{emoji} slipped through");
        }
    }

    #[test]
    fn the_glyph_vocabulary_is_not_emoji() {
        // ✓ and ✗ share a Unicode block with ✅ and ❌; the design system uses them and a
        // range-only check would ban the product's own vocabulary.
        for glyph in GLYPH_VOCABULARY {
            let source = format!("let s = \"{glyph} Copied to your clipboard\";");
            assert_eq!(rules(&source), Vec::<Rule>::new(), "{glyph} flagged as emoji");
        }
    }

    #[test]
    fn real_model_names_are_allowed_but_the_brand_is_not() {
        assert_eq!(rules(r#"let s = "GPT-5.3 Codex";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "gpt-5.3-codex";"#), Vec::<Rule>::new());
        assert_eq!(
            rules(r#"let s = "Model: GPT-5.3 Codex, fastest";"#),
            Vec::<Rule>::new()
        );
        // A model id in the same string does not launder a real leak.
        assert_eq!(
            rules(r#"let s = "GPT-5.3 Codex is what Codex uses";"#),
            vec![Rule::Codex]
        );
        // Lowercase `codex` is a functional slug (paths, config keys), never policed.
        assert_eq!(rules(r#"let s = "~/.codex/config.toml";"#), Vec::<Rule>::new());
        // A longer identifier is not the brand word.
        assert_eq!(rules(r#"let s = "CodexAuth";"#), Vec::<Rule>::new());
    }

    #[test]
    fn openai_is_allowed_where_rule_six_allows_it() {
        assert_eq!(
            rules(r#"let s = "Paste your OpenAI key";"#),
            Vec::<Rule>::new()
        );
        assert_eq!(
            rules(r#"let s = "Any OpenAI-compatible server";"#),
            Vec::<Rule>::new()
        );
        assert_eq!(rules(r#"let s = "Answered by OpenAI";"#), vec![Rule::OpenAi]);
    }

    #[test]
    fn the_auto_rule_reads_prose_only() {
        // Wire values and slugs stay lowercase.
        assert_eq!(rules(r#"let s = "auto";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "model=auto";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "auto-review is on";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "gpt-auto/mini here";"#), Vec::<Rule>::new());
        // The display name is what prose uses.
        assert_eq!(rules(r#"let s = "Auto picks a model";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "we used auto instead";"#), vec![Rule::AutoSlug]);
    }

    #[test]
    fn a_single_character_marker_has_no_tone() {
        assert_eq!(rules(r#"let s = "!";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "Done!";"#), vec![Rule::Exclamation]);
    }

    #[test]
    fn comments_and_test_modules_are_not_copy() {
        let source = "\
// \"Codex is thinking\"
/// \"Powered by OpenAI\"
/* \"Faster than Cursor\" */
let ok = \"Loading usage\";
#[cfg(test)]
mod tests {
    const BAD: &str = \"Codex is thinking\";
}
";
        assert_eq!(rules(source), Vec::<Rule>::new());
    }

    #[test]
    fn lifetimes_do_not_swallow_the_rest_of_the_file() {
        // `'a` has no closing quote; a naive scanner treats everything after it as a literal
        // and silently guards nothing from that point on.
        let source = "impl<'a> View<'a> { fn t(&'a self) -> &'a str { \"Codex here\" } }";
        assert_eq!(rules(source), vec![Rule::Codex]);
    }

    #[test]
    fn raw_strings_are_read_too() {
        assert_eq!(rules("let s = r\"Codex here\";"), vec![Rule::Codex]);
        assert_eq!(rules("let s = r#\"Codex \"quoted\" here\"#;"), vec![Rule::Codex]);
    }

    #[test]
    fn line_numbers_point_at_the_literal() {
        let source = "fn a() {}\n\nlet s = \"Signed in!\";\n";
        let found = violations(source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 3);
        assert_eq!(found[0].literal, "Signed in!");
    }

    #[test]
    fn the_allowlist_actually_suppresses() {
        // Same literal as the shipped allowlist entry, so this fails if the entry is dropped
        // without the copy being fixed.
        assert_eq!(rules(r#"let s = "OpenAI";"#), Vec::<Rule>::new());
        assert_eq!(rules(r#"let s = "OpenAI is fast";"#), vec![Rule::OpenAi]);
    }

    #[test]
    fn the_guard_reads_the_files_it_claims_to() {
        let files = guarded_files();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for expected in ["byteask_a11y.rs", "byteask_account.rs"] {
            assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
        }
        assert!(
            !names.contains(&"byteask_copy_guard_tests.rs".to_string()),
            "the guard must not read itself"
        );
    }
}
