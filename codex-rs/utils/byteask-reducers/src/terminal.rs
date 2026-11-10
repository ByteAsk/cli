//! Terminal-equivalent transforms: what a person would see on the screen.

/// Result of a reduction. `text` equals the input when nothing fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduced {
    pub text: String,
    /// Names of the transforms that changed something, in the order applied.
    pub fired: Vec<&'static str>,
}

impl Reduced {
    pub fn changed(&self) -> bool {
        !self.fired.is_empty()
    }
}

/// Outputs smaller than this are never touched: nothing worth saving, and tiny outputs
/// are where an unexpected transform would be most confusing.
pub const MIN_BYTES: usize = 512;

/// Strip ANSI escapes and collapse bare carriage-return overwrites.
///
/// `ansi` and `cr` are the policy switches. Idempotent: reducing a reduced text is a no-op.
pub fn reduce_terminal(input: &str, ansi: bool, cr: bool) -> Reduced {
    if input.len() < MIN_BYTES || crate::is_reduced(input) {
        return Reduced { text: input.to_string(), fired: Vec::new() };
    }
    let mut fired = Vec::new();
    let mut text = input.to_string();
    if ansi && text.contains('\u{1b}') {
        let stripped = strip_ansi(&text);
        if stripped.len() != text.len() {
            text = stripped;
            fired.push("ansi");
        }
    }
    if cr && text.contains('\r') {
        let collapsed = collapse_carriage_returns(&text);
        if collapsed.len() != text.len() {
            text = collapsed;
            fired.push("cr");
        }
    }
    Reduced { text, fired }
}

/// Remove CSI (`ESC [ … final`), OSC (`ESC ] … BEL | ESC \`) and two-byte escapes.
/// An escape that never terminates is left exactly as it was.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\u{1b}' {
            out.push(c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('[') => {
                // parameters 0x30-0x3F, intermediates 0x20-0x2F, one final 0x40-0x7E
                let mut j = i + 2;
                while j < chars.len() && matches!(chars[j], '\u{20}'..='\u{3f}') {
                    j += 1;
                }
                if j < chars.len() && matches!(chars[j], '\u{40}'..='\u{7e}') {
                    i = j + 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            Some(']') => {
                let mut j = i + 2;
                let mut end = None;
                while j < chars.len() {
                    if chars[j] == '\u{07}' {
                        end = Some(j + 1);
                        break;
                    }
                    if chars[j] == '\u{1b}' && chars.get(j + 1) == Some(&'\\') {
                        end = Some(j + 2);
                        break;
                    }
                    if chars[j] == '\n' {
                        break; // never swallow across a line: not a real OSC
                    }
                    j += 1;
                }
                match end {
                    Some(e) => i = e,
                    None => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            // ESC followed by one char in 0x40-0x5F or a charset selector: a 2-byte escape.
            Some(next) if matches!(*next, '\u{40}'..='\u{5f}' | '(' | ')' | '=' | '>' | '7' | '8') => {
                let skip = if matches!(*next, '(' | ')') { 3 } else { 2 };
                i = (i + skip).min(chars.len());
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Replay bare `\r` the way a terminal does: the cursor returns to column 0 and later
/// text overwrites what was there. `\r\n` is a line ending and is never touched.
pub fn collapse_carriage_returns(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for (idx, line) in input.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let (body, crlf) = match line.strip_suffix('\r') {
            Some(b) => (b, true),
            None => (line, false),
        };
        if body.contains('\r') {
            let mut screen: Vec<char> = Vec::new();
            let mut col = 0usize;
            for ch in body.chars() {
                if ch == '\r' {
                    col = 0;
                } else {
                    if col < screen.len() {
                        screen[col] = ch;
                    } else {
                        screen.push(ch);
                    }
                    col += 1;
                }
            }
            out.extend(screen);
        } else {
            out.push_str(body);
        }
        if crlf {
            out.push('\r');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn pad(s: &str) -> String {
        format!("{s}\n{}", "filler line that carries no escapes at all\n".repeat(20))
    }

    #[test]
    fn small_outputs_are_never_touched() {
        let small = "\u{1b}[31mred\u{1b}[0m";
        let r = reduce_terminal(small, true, true);
        assert_eq!(r.text, small);
        assert!(!r.changed());
    }

    #[test]
    fn colour_codes_go_and_the_text_stays() {
        let r = reduce_terminal(&pad("\u{1b}[1;31merror\u{1b}[0m: \u{1b}[38;5;208mfoo.c\u{1b}[m:12"), true, true);
        assert!(r.text.starts_with("error: foo.c:12\n"));
        assert_eq!(r.fired, vec!["ansi"]);
    }

    #[test]
    fn osc_hyperlinks_and_titles_go() {
        let s = pad("\u{1b}]0;window title\u{07}\u{1b}]8;;https://x.y\u{1b}\\link\u{1b}]8;;\u{1b}\\ done");
        assert!(reduce_terminal(&s, true, true).text.starts_with("link done\n"));
    }

    #[test]
    fn an_unterminated_escape_is_left_alone() {
        let s = pad("before \u{1b}[31");
        let r = reduce_terminal(&s, true, true);
        assert!(r.text.starts_with("before \u{1b}[31\n"));
    }

    #[test]
    fn a_progress_bar_collapses_to_its_final_state() {
        let s = pad("Downloading   5%\rDownloading  50%\rDownloading 100%");
        let r = reduce_terminal(&s, true, true);
        assert!(r.text.starts_with("Downloading 100%\n"));
        assert_eq!(r.fired, vec!["cr"]);
    }

    #[test]
    fn a_shorter_overwrite_keeps_the_tail_like_a_real_terminal() {
        assert_eq!(collapse_carriage_returns("abcdef\rXY"), "XYcdef");
    }

    #[test]
    fn crlf_line_endings_are_never_touched() {
        let s = pad("one\r\ntwo\r\nthree\r");
        let r = reduce_terminal(&s, true, true);
        assert_eq!(r.text, s);
        assert!(!r.changed());
    }

    #[test]
    fn switches_are_honoured() {
        let s = pad("\u{1b}[31mx\u{1b}[0m a\rb");
        assert_eq!(reduce_terminal(&s, false, false).text, s);
        assert_eq!(reduce_terminal(&s, true, false).fired, vec!["ansi"]);
        assert_eq!(reduce_terminal(&s, false, true).fired, vec!["cr"]);
    }

    #[test]
    fn it_is_idempotent_and_deterministic() {
        let s = pad("\u{1b}[32mok\u{1b}[0m 1%\r 99%\r100%");
        let once = reduce_terminal(&s, true, true);
        let twice = reduce_terminal(&once.text, true, true);
        assert_eq!(once.text, twice.text);
        assert!(!twice.changed());
        assert_eq!(once, reduce_terminal(&s, true, true));
    }

    #[test]
    fn multibyte_text_survives() {
        let s = pad("\u{1b}[1m日本語 ✓ naïve\u{1b}[0m");
        assert!(reduce_terminal(&s, true, true).text.starts_with("日本語 ✓ naïve\n"));
    }
}
