//! Field paths used by the comparison rules.
//!
//! Syntax is RFC 6901 JSON Pointer (`/out/actions/0/qty`, `~1` for `/` and
//! `~0` for `~` inside a key), plus one extension: a segment that is exactly
//! `*` matches any single object key or array index. `""` is the whole record.
//! A key literally named `*` cannot be addressed; that is a deliberate limit,
//! not an oversight, because no recorded format we target uses one.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Any,
}

/// A parsed path pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathPattern {
    raw: String,
    segments: Vec<Segment>,
}

impl PathPattern {
    /// Parse a pointer pattern. Errors name what is wrong in plain words.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Ok(Self {
                raw: String::new(),
                segments: Vec::new(),
            });
        }
        let Some(rest) = raw.strip_prefix('/') else {
            return Err(format!(
                "path {raw:?} must start with '/' (JSON Pointer syntax, e.g. \"/out/qty\"), or be \"\" for the whole record"
            ));
        };
        let mut segments = Vec::new();
        for part in rest.split('/') {
            if part == "*" {
                segments.push(Segment::Any);
            } else {
                segments.push(Segment::Literal(
                    unescape(part).map_err(|e| format!("path {raw:?}: {e}"))?,
                ));
            }
        }
        Ok(Self {
            raw: raw.to_string(),
            segments,
        })
    }

    /// True when the pattern contains a `*` segment.
    pub fn has_wildcard(&self) -> bool {
        self.segments.iter().any(|s| matches!(s, Segment::Any))
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Does this pattern match the concrete path (one string per segment;
    /// array indices are their decimal text)?
    pub fn matches(&self, path: &[String]) -> bool {
        self.segments.len() == path.len()
            && self
                .segments
                .iter()
                .zip(path)
                .all(|(seg, part)| match seg {
                    Segment::Any => true,
                    Segment::Literal(lit) => lit == part,
                })
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn unescape(part: &str) -> Result<String, String> {
    let mut out = String::with_capacity(part.len());
    let mut chars = part.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                Some(other) => {
                    return Err(format!("'~' must be followed by 0 or 1 (got {other:?})"));
                }
                None => return Err("'~' must be followed by 0 or 1 (got end of segment)".to_string()),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Render a concrete path as a JSON Pointer string.
pub fn render(path: &[String]) -> String {
    let mut out = String::new();
    for part in path {
        out.push('/');
        for c in part.chars() {
            match c {
                '~' => out.push_str("~0"),
                '/' => out.push_str("~1"),
                c => out.push(c),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn p(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn literal_and_wildcard_segments_match_exactly_one_level() {
        let pat = PathPattern::parse("/out/*/qty").expect("parse");
        assert!(pat.matches(&p(&["out", "0", "qty"])));
        assert!(pat.matches(&p(&["out", "x", "qty"])));
        assert!(!pat.matches(&p(&["out", "qty"])), "a * is exactly one segment");
        assert!(!pat.matches(&p(&["out", "0", "1", "qty"])));
        assert!(!pat.matches(&p(&["out", "0", "px"])));
        assert!(pat.has_wildcard());
    }

    #[test]
    fn empty_pattern_is_the_whole_record() {
        let pat = PathPattern::parse("").expect("parse");
        assert!(pat.matches(&[]));
        assert!(!pat.matches(&p(&["a"])));
    }

    #[test]
    fn escapes_follow_rfc_6901_and_round_trip_through_render() {
        let pat = PathPattern::parse("/a~1b/c~0d").expect("parse");
        assert!(pat.matches(&p(&["a/b", "c~d"])));
        assert_eq!(render(&p(&["a/b", "c~d"])), "/a~1b/c~0d");
        assert_eq!(render(&[]), "");
        // serde_json's own pointer lookup agrees with our rendering.
        let v = serde_json::json!({"a/b": {"c~d": 7}});
        assert_eq!(v.pointer(&render(&p(&["a/b", "c~d"]))), Some(&serde_json::json!(7)));
    }

    #[test]
    fn malformed_patterns_are_refused_with_a_reason() {
        let no_slash = PathPattern::parse("out/qty").expect_err("must refuse");
        assert!(no_slash.contains("must start with '/'"), "{no_slash}");
        let bad_escape = PathPattern::parse("/a~2").expect_err("must refuse");
        assert!(bad_escape.contains("'~' must be followed by 0 or 1"), "{bad_escape}");
        let trailing = PathPattern::parse("/a~").expect_err("must refuse");
        assert!(trailing.contains("end of segment"), "{trailing}");
    }
}
