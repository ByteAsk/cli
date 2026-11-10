use std::path::Path;
use std::process::Command;

use pretty_assertions::assert_eq;

use super::*;

pub(crate) fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

const HEADER: &str = r#"#pragma once
#include <string>
namespace lib {
class Shape {
 public:
  virtual ~Shape();
  virtual double area() const = 0;
};
class Square : public Shape {
 public:
  double area() const override { return s * s; }
  double s = 1;
};
class Circle : public Shape {
 public:
  double area() const override { return 3.14 * r * r; }
  double r = 1;
};
class Widget {
 public:
  int size() const;
  bool empty() const;
  const char* data() const;
  int begin() const;
  int end() const;
};
class Panel {
 public:
  int size() const;
  bool empty() const;
  const char* data() const;
  int begin() const;
  int end() const;
};
std::string describe(const Widget& w);
std::string describe(const Panel& p);
inline long to_utc(long t) {
  long off = zone_offset(t);
  if (off > 0) { t -= off; }
  return clamp_range(t, 0, max_time());
}
inline long to_local(long t) {
  long off = zone_offset(t);
  if (off > 0) { t += off; }
  return clamp_range(t, 0, max_time());
}
#if LIB_EXCEPTIONS
bool is_valid(const Widget& w);
#else
int is_valid(const Widget& w);
#endif
inline int parse_flag(char c) {
  switch (c) {
  case 'a': return 1;
  case 'b': return 2;
  case '-': return 3;
  case '_': return 4;
  default: return 0;
  }
}
}  // namespace lib
"#;

/// A small repository with one of each pattern, committed, then edited.
pub(crate) fn fixture() -> Option<tempfile::TempDir> {
    let dir = fixture_base()?;
    apply_edits(dir.path())?;
    Some(dir)
}

/// The fixture repository as committed (no edits yet).
pub(crate) fn fixture_base() -> Option<tempfile::TempDir> {
    let dir = tempfile::tempdir().ok()?;
    fixture_base_in(dir.path())?;
    Some(dir)
}

/// [`fixture_base`] created at `root` (which may not exist yet).
pub(crate) fn fixture_base_in(root: &Path) -> Option<()> {
    std::fs::create_dir_all(root.join("include/lib")).ok()?;
    std::fs::create_dir_all(root.join("single")).ok()?;
    std::fs::create_dir_all(root.join("src")).ok()?;
    std::fs::create_dir_all(root.join("tests")).ok()?;
    std::fs::create_dir_all(root.join("docs")).ok()?;
    std::fs::create_dir_all(root.join(".github/workflows")).ok()?;
    std::fs::write(root.join("include/lib/widget.h"), HEADER).ok()?;
    std::fs::write(
        root.join("single/lib.hpp"),
        format!("// combined single-header build, generated: do not edit\n{HEADER}"),
    )
    .ok()?;
    std::fs::write(
        root.join("src/use.cc"),
        "#include \"lib/widget.h\"\nlong run() {\n  return lib::to_utc(5);\n}\n",
    )
    .ok()?;
    std::fs::write(
        root.join("tests/widget_test.cc"),
        "#include \"lib/widget.h\"\nTEST(Widget, Utc) {\n  EXPECT_EQ(lib::to_utc(0), 0);\n}\n",
    )
    .ok()?;
    std::fs::write(
        root.join("docs/api.md"),
        "# API\n\n`to_utc` converts a local time.\n",
    )
    .ok()?;
    std::fs::write(
        root.join(".github/workflows/ci.yml"),
        "jobs:\n  noexc:\n    run: cmake -DLIB_EXCEPTIONS=OFF ..\n",
    )
    .ok()?;
    if !git(root, &["init", "-q"])
        || !git(root, &["add", "-A"])
        || !git(root, &["commit", "-q", "-m", "base"])
    {
        return None;
    }
    Some(())
}

/// The edits every facet reacts to.
pub(crate) fn apply_edits(root: &Path) -> Option<()> {
    let edited = HEADER
        .replace("  if (off > 0) { t -= off; }", "  if (off > 0 && t > off) { t -= off; }")
        .replace(
            "std::string describe(const Panel& p);",
            "std::string describe(const Panel& p);\nstd::ostream& operator<<(std::ostream& os, const Widget& w);",
        )
        .replace("  case '-': return 3;", "  case '-': return 5;")
        .replace("bool is_valid(const Widget& w);", "bool is_valid(const Widget& w) noexcept;")
        .replace("double area() const override { return s * s; }", "double area() const override { return s * s * 1.0; }");
    std::fs::write(root.join("include/lib/widget.h"), edited).ok()?;
    Some(())
}

fn request(root: &Path) -> ImpactRequest {
    ImpactRequest {
        root: root.to_path_buf(),
        base_rev: "HEAD".to_string(),
        source: Source::Diff,
        budget: Duration::from_secs(60),
        use_clangd: false,
        max_items: 40,
        preexisting_untracked: Vec::new(),
    }
}

fn find<'a>(report: &'a ImpactReport, facet: Facet, needle: &str) -> Option<&'a Item> {
    report.items.iter().find(|item| {
        item.facet == facet && (item.text.contains(needle) || item.path.contains(needle))
    })
}

#[tokio::test]
async fn the_checklist_lists_every_facet_on_a_fixture() {
    let Some(dir) = fixture() else {
        eprintln!("skipped: git not available");
        return;
    };
    let report = analyze(&request(dir.path())).await;
    let dump = format!("{:#?}", report.items);
    assert_eq!(report.files_changed, 1, "{dump}");
    let changed: Vec<String> = report
        .changed
        .iter()
        .map(|c| c.entity.name.clone())
        .collect();
    for name in ["to_utc", "operator<<", "parse_flag", "is_valid", "area"] {
        assert!(changed.contains(&name.to_string()), "{changed:?}");
    }
    let added: Vec<&str> = report
        .changed
        .iter()
        .filter(|c| c.added)
        .map(|c| c.entity.name.as_str())
        .collect();
    assert_eq!(added, vec!["operator<<"], "{:#?}", report.changed);

    assert!(
        find(&report, Facet::LookAlikes, "to_local").is_some(),
        "{dump}"
    );
    assert!(find(&report, Facet::Peers, "Panel").is_some(), "{dump}");
    let switch = find(&report, Facet::Switch, "parse")
        .or_else(|| find(&report, Facet::Switch, "switch (c)"));
    let switch = switch.expect(&dump);
    assert!(
        switch.text.contains("'-'") && switch.text.contains("'_'"),
        "{}",
        switch.text
    );
    assert!(
        find(&report, Facet::Configs, "int is_valid").is_some()
            || find(&report, Facet::Configs, "is_valid").is_some(),
        "{dump}"
    );
    assert!(find(&report, Facet::Configs, "ci.yml").is_some(), "{dump}");
    assert!(
        find(&report, Facet::Copies, "single/lib.hpp").is_some(),
        "{dump}"
    );
    assert!(
        find(&report, Facet::Overrides, "Circle").is_some(),
        "{dump}"
    );
    assert!(
        find(&report, Facet::Callers, "src/use.cc").is_some(),
        "{dump}"
    );
    assert!(
        find(&report, Facet::Tests, "tests/widget_test.cc").is_some(),
        "{dump}"
    );
    assert!(
        find(&report, Facet::Docs, "docs/api.md").is_some(),
        "{dump}"
    );
    // The generated copy is not reported as a look-alike or overload source.
    assert!(
        !report
            .items
            .iter()
            .any(|i| i.path == "single/lib.hpp" && i.facet != Facet::Copies),
        "{dump}"
    );

    let mut numbering = Numbering::default();
    let text = render(&report, &mut numbering, "cpp_impact", None);
    assert!(text.len() <= RENDER_CAP_BYTES + 200, "{} bytes", text.len());
    assert!(text.contains("[1] "), "{text}");
    assert!(text.contains(DISPOSITION_RULE), "{text}");
    // Numbers are stable on re-render.
    let again = render(&report, &mut numbering, "cpp_impact", None);
    assert_eq!(text, again);
}

#[tokio::test]
async fn an_unchanged_tree_gives_an_empty_checklist() {
    let Some(dir) = fixture() else {
        eprintln!("skipped: git not available");
        return;
    };
    assert!(git(dir.path(), &["checkout", "--", "."]));
    let report = analyze(&request(dir.path())).await;
    assert_eq!(report.files_changed, 0);
    assert!(report.items.is_empty(), "{:#?}", report.items);
    let mut numbering = Numbering::default();
    assert!(render(&report, &mut numbering, "cpp_impact", None).contains("checklist is empty"));
}

#[tokio::test]
async fn named_symbols_work_without_a_diff() {
    let Some(dir) = fixture() else {
        eprintln!("skipped: git not available");
        return;
    };
    assert!(git(dir.path(), &["checkout", "--", "."]));
    let mut req = request(dir.path());
    req.source = Source::Symbols(vec!["lib::to_utc".to_string()]);
    let report = analyze(&req).await;
    assert!(
        report.changed.iter().any(|c| c.entity.name == "to_utc"),
        "{:#?}",
        report.changed
    );
    assert!(
        find(&report, Facet::LookAlikes, "to_local").is_some(),
        "{:#?}",
        report.items
    );
    assert!(
        find(&report, Facet::Callers, "src/use.cc").is_some(),
        "{:#?}",
        report.items
    );
}

#[test]
fn base_types_and_template_arguments() {
    assert_eq!(base_type("const lib::Widget&"), "Widget");
    assert_eq!(base_type("std::vector<int>&&"), "vector");
    assert_eq!(type_args("result<dom::node>&"), vec!["node".to_string()]);
    assert!(looks_like_build_output("build/x.cc"));
    assert!(looks_like_build_output(".scratch/x.cc"));
    assert!(!looks_like_build_output("src/x.cc"));
}
