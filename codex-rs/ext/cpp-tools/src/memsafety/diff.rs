//! A small unified-diff reader for the patch-shape critic: files, hunks,
//! added/removed lines with their new/old line numbers, and the function each
//! changed line sits in (from git's hunk-header context and function-looking
//! lines inside the hunk).

/// One changed or context line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Line {
    Context(String),
    Added(String),
    Removed(String),
}

/// A changed line with where it sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) added: bool,
    pub(crate) text: String,
    /// New-file line for an added line, old-file line for a removed one.
    pub(crate) line: u32,
    /// The enclosing function, when one could be told.
    pub(crate) function: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub(crate) old_start: u32,
    pub(crate) new_start: u32,
    /// git's function context after the second `@@`.
    pub(crate) context: String,
    pub(crate) lines: Vec<Line>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileDiff {
    /// The new path (the old one for a deletion), without `a/`/`b/`.
    pub(crate) path: String,
    pub(crate) hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Every added or removed line, with its line number and function.
    pub(crate) fn changes(&self) -> Vec<Change> {
        let mut out = Vec::new();
        for hunk in &self.hunks {
            let mut function = function_name(&hunk.context);
            let mut old_line = hunk.old_start;
            let mut new_line = hunk.new_start;
            for line in &hunk.lines {
                match line {
                    Line::Context(text) => {
                        if let Some(name) = definition_name(text) {
                            function = Some(name);
                        }
                        old_line += 1;
                        new_line += 1;
                    }
                    Line::Added(text) => {
                        if let Some(name) = definition_name(text) {
                            function = Some(name);
                        }
                        out.push(Change {
                            added: true,
                            text: text.clone(),
                            line: new_line,
                            function: function.clone(),
                        });
                        new_line += 1;
                    }
                    Line::Removed(text) => {
                        if let Some(name) = definition_name(text) {
                            function = Some(name);
                        }
                        out.push(Change {
                            added: false,
                            text: text.clone(),
                            line: old_line,
                            function: function.clone(),
                        });
                        old_line += 1;
                    }
                }
            }
        }
        out
    }
}

/// Parse `git diff` output. Binary and mode-only entries produce a file with
/// no hunks.
pub(crate) fn parse(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut minus_path = String::new();
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            // `a/x b/x`: the b-side is the default until `+++` says otherwise.
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            current = Some(FileDiff {
                path,
                hunks: Vec::new(),
            });
            continue;
        }
        if let Some(rest) = raw.strip_prefix("--- ") {
            if current.as_ref().is_some_and(|f| f.hunks.is_empty()) {
                minus_path = strip_side(rest);
                continue;
            }
        }
        if let Some(rest) = raw.strip_prefix("+++ ") {
            if let Some(file) = current.as_mut().filter(|f| f.hunks.is_empty()) {
                let plus = strip_side(rest);
                file.path = if plus == "/dev/null" {
                    minus_path.clone()
                } else {
                    plus
                };
                continue;
            }
        }
        if let Some(rest) = raw.strip_prefix("@@ ") {
            let Some(file) = current.as_mut() else {
                continue;
            };
            let (ranges, context) = match rest.split_once(" @@") {
                Some((ranges, context)) => (ranges, context.trim().to_string()),
                None => (rest, String::new()),
            };
            let mut old_start = 0;
            let mut new_start = 0;
            for part in ranges.split_whitespace() {
                let number = |p: &str| {
                    p.split(',')
                        .next()
                        .and_then(|n| n.parse::<u32>().ok())
                        .unwrap_or(0)
                };
                if let Some(old) = part.strip_prefix('-') {
                    old_start = number(old);
                } else if let Some(new) = part.strip_prefix('+') {
                    new_start = number(new);
                }
            }
            file.hunks.push(Hunk {
                old_start,
                new_start,
                context,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = current.as_mut().and_then(|f| f.hunks.last_mut()) else {
            continue;
        };
        if let Some(text) = raw.strip_prefix('+') {
            hunk.lines.push(Line::Added(text.to_string()));
        } else if let Some(text) = raw.strip_prefix('-') {
            hunk.lines.push(Line::Removed(text.to_string()));
        } else if let Some(text) = raw.strip_prefix(' ') {
            hunk.lines.push(Line::Context(text.to_string()));
        } else if raw.is_empty() {
            hunk.lines.push(Line::Context(String::new()));
        }
        // `\ No newline at end of file` and anything else: ignored.
    }
    if let Some(file) = current.take() {
        files.push(file);
    }
    files
}

fn strip_side(path: &str) -> String {
    let path = path.split('\t').next().unwrap_or(path).trim();
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// The function a hunk-header context names (`static int foo(int x)` →
/// `foo`), if it looks like a definition.
pub(crate) fn function_name(context: &str) -> Option<String> {
    let context = context.trim();
    if context.is_empty() {
        return None;
    }
    let open = context.find('(')?;
    let head = context[..open].trim_end();
    let name: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':' || *c == '~')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let name = name.trim_start_matches(':').to_string();
    const NOT_FUNCTIONS: &[&str] = &[
        "if", "for", "while", "switch", "return", "sizeof", "catch", "defined", "else",
    ];
    if name.is_empty()
        || NOT_FUNCTIONS.contains(&name.as_str())
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(name)
}

/// A line that starts a function definition at column 0: `type name(args)` or
/// the GNU style `name (args)` with the return type on the line before, and
/// not a declaration (`;`), a macro or a control statement.
fn definition_name(line: &str) -> Option<String> {
    let first = line.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let trimmed = line.trim_end();
    if trimmed.ends_with(';') || trimmed.ends_with(',') || trimmed.starts_with("#") {
        return None;
    }
    if !trimmed.contains('(') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    for keyword in ["typedef ", "return ", "else", "case ", "goto "] {
        if lower.starts_with(keyword) {
            return None;
        }
    }
    function_name(trimmed)
}

/// Is `path` a C/C++ source or header?
pub(crate) fn is_cxx_path(path: &str) -> bool {
    const EXTS: &[&str] = &[
        "c", "cc", "cpp", "cxx", "c++", "h", "hh", "hpp", "hxx", "h++", "inc", "ipp", "tpp", "inl",
    ];
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const DIFF: &str = "\
diff --git a/src/reader.c b/src/reader.c
index 1111111..2222222 100644
--- a/src/reader.c
+++ b/src/reader.c
@@ -10,6 +10,8 @@ parse_header (Chain *dat, Doc *doc)
   int n = read_len (dat);
   char buf[64];
+  if (n > 64)
+    return -1;
   memcpy (buf, dat->p, n);
-  free (dat->tmp);
   return 0;
 }
@@ -40,3 +42,4 @@ int other(void)
 static int
 helper (int x)
 {
+  x++;
";

    #[test]
    fn reads_files_hunks_and_functions() {
        let files = parse(DIFF);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/reader.c");
        let changes = files[0].changes();
        let summary: Vec<(bool, u32, Option<String>)> = changes
            .iter()
            .map(|c| (c.added, c.line, c.function.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (true, 12, Some("parse_header".to_string())),
                (true, 13, Some("parse_header".to_string())),
                (false, 13, Some("parse_header".to_string())),
                (true, 45, Some("helper".to_string())),
            ]
        );
    }

    #[test]
    fn function_names_from_contexts() {
        assert_eq!(function_name("static int foo(int x)").as_deref(), Some("foo"));
        assert_eq!(function_name("void ns::A::run(const B &b) const").as_deref(), Some("ns::A::run"));
        assert_eq!(function_name("if (x)"), None);
        assert_eq!(function_name("struct thing {"), None);
    }

    #[test]
    fn deleted_and_new_files_keep_their_path() {
        let deleted = "diff --git a/x/old.c b/x/old.c\ndeleted file mode 100644\n--- a/x/old.c\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-int a;\n";
        assert_eq!(parse(deleted)[0].path, "x/old.c");
        let new = "diff --git a/y.c b/y.c\nnew file mode 100644\n--- /dev/null\n+++ b/y.c\n@@ -0,0 +1,1 @@\n+int b;\n";
        assert_eq!(parse(new)[0].path, "y.c");
    }

    #[test]
    fn source_extensions() {
        assert!(is_cxx_path("a/b.cpp"));
        assert!(is_cxx_path("x.H"));
        assert!(!is_cxx_path("CMakeLists.txt"));
    }
}
