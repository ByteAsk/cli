//! The registry: which reducers run on which output, in what order, and whether a stage
//! is kept.
//!
//! ```text
//!  raw output + ctx{command, path_exists} + policy
//!     [1] ansi + cr ........ every output >= 512 bytes
//!     [2] family reducers .. only on a POSITIVE family match, each one individually
//!                            switched from the policy object
//!     [3] accept gate ...... a stage is kept only if it saved >= 10% AND >= 200 bytes
//! ```
//!
//! Budgets and the spill are the caller's job: this module is pure text -> text.
//! Pure, deterministic, idempotent, linear; a panic in a reducer means passthrough.

use crate::family::Family;
use crate::policy::Policy;
use crate::terminal::MIN_BYTES;
use crate::terminal::Reduced;
use crate::terminal::reduce_terminal;

/// What the caller knows about the output.
pub struct Ctx<'a> {
    /// The command line that produced the output, when there is one.
    pub command: Option<&'a str>,
    /// Does this path exist? Used only to prove that `path:line:text` really starts with
    /// a path. `None` disables the reducers that need it.
    pub path_exists: Option<&'a dyn Fn(&str) -> bool>,
}

/// Result of the full pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub text: String,
    pub fired: Vec<&'static str>,
    pub family: Family,
    /// True when something other than a terminal-equivalent transform fired: content was
    /// removed, so the original must be recoverable.
    pub lossy: bool,
}

impl Outcome {
    pub fn changed(&self) -> bool {
        !self.fired.is_empty()
    }
}

const GATE_MIN_BYTES: usize = 200;

/// Keep a stage only if it saved >= 10% AND >= 200 bytes.
fn accept(before: &str, after: &str) -> bool {
    let saved = before.len().saturating_sub(after.len());
    saved >= GATE_MIN_BYTES && saved * 10 >= before.len()
}

type Stage<'a> = (&'static str, bool, Box<dyn Fn(&str) -> Option<String> + 'a>);

fn stages<'a>(family: Family, ctx: &'a Ctx<'a>) -> Vec<Stage<'a>> {
    use crate::gated as g;
    use crate::lossless as l;
    use crate::nearloss as n;
    // (name, lossy, function). Order matters: structure first, then counts, then folds.
    let mut v: Vec<Stage<'a>> = Vec::new();
    let mut add = |name: &'static str, lossy: bool, f: Box<dyn Fn(&str) -> Option<String> + 'a>| {
        v.push((name, lossy, f));
    };
    match family {
        Family::Unknown => {}
        Family::Search => {
            if let Some(exists) = ctx.path_exists {
                add("search-group", false, Box::new(move |t| l::search_group(t, exists)));
            }
            add("blob-line", true, Box::new(g::blob_line));
        }
        Family::Listing => {
            add("vendor-ls", true, Box::new(g::vendor_ls));
            add("path-fold", false, Box::new(l::path_fold));
        }
        Family::Diff => {
            add("gen-diff", true, Box::new(g::gen_diff));
        }
        Family::Build => {
            add("ws", false, Box::new(l::ws));
            add("rle", false, Box::new(l::rle));
            add("progress", true, Box::new(n::progress));
            add("cmake-conf", true, Box::new(n::cmake_conf));
            add("incl-chain", true, Box::new(n::incl_chain));
            add("warn-group", true, Box::new(n::warn_group));
            add("cxx-templates", true, Box::new(g::cxx_templates));
            add("cxx-candidates", true, Box::new(g::cxx_candidates));
            add("link-group", true, Box::new(g::link_group));
        }
        Family::Test => {
            add("ws", false, Box::new(l::ws));
            add("rle", false, Box::new(l::rle));
            add("progress", true, Box::new(n::progress));
            add("tests", true, Box::new(n::tests));
            add("stack-fold", true, Box::new(g::stack_fold));
        }
        Family::Install => {
            add("ws", false, Box::new(l::ws));
            add("rle", false, Box::new(l::rle));
            add("progress", true, Box::new(n::progress));
            add("pkg", true, Box::new(n::pkg));
        }
        Family::Log => {
            add("ws", false, Box::new(l::ws));
            add("rle", false, Box::new(l::rle));
            add("log-prefix", true, Box::new(g::log_prefix));
            add("stack-fold", true, Box::new(g::stack_fold));
        }
        Family::Network => {
            add("html-text", true, Box::new(g::html_text));
            add("json-array", true, Box::new(g::json_array));
            add("json-min", true, Box::new(n::json_min));
            add("table", true, Box::new(g::table));
            add("blob-line", true, Box::new(g::blob_line));
        }
    }
    v
}

/// Run the pipeline. Never panics outward, never grows the text.
pub fn reduce(input: &str, ctx: &Ctx<'_>, policy: &Policy) -> Outcome {
    let family = ctx.command.map(crate::family::classify).unwrap_or(Family::Unknown);
    let passthrough = |family| Outcome {
        text: input.to_string(),
        fired: Vec::new(),
        family,
        lossy: false,
    };
    if policy.brake || input.len() < MIN_BYTES || crate::is_reduced(input) {
        return passthrough(family);
    }
    let Reduced { mut text, mut fired } = reduce_terminal(input, policy.ansi, policy.cr);
    let mut lossy = false;
    for (name, stage_lossy, f) in stages(family, ctx) {
        if !policy.reducer_on(name) {
            continue;
        }
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&text)));
        let Ok(Some(next)) = run else { continue };
        if accept(&text, &next) {
            text = next;
            fired.push(name);
            lossy |= stage_lossy;
        }
    }
    Outcome { text, fired, family, lossy }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn all_on() -> Policy {
        let mut p = Policy::default();
        for name in crate::policy::DARK_REDUCERS {
            p.enabled.insert((*name).to_string());
        }
        p
    }

    fn cargo_test_output() -> String {
        let mut s = String::from("running 60 tests\n");
        for i in 0..60 {
            s.push_str(&format!("test module::case_number_{i} ... ok\n"));
        }
        s.push_str("\ntest result: ok. 60 passed; 0 failed\n");
        s
    }

    #[test]
    fn an_unknown_command_gets_only_the_terminal_transforms() {
        let text = cargo_test_output();
        let ctx = Ctx { command: Some("bash -lc 'cat log.txt'"), path_exists: None };
        let out = reduce(&text, &ctx, &all_on());
        assert_eq!(out.family, Family::Unknown);
        assert_eq!(out.text, text);
        assert!(!out.lossy);
    }

    #[test]
    fn dark_reducers_stay_off_by_default() {
        let text = cargo_test_output();
        let ctx = Ctx { command: Some("cargo test"), path_exists: None };
        let out = reduce(&text, &ctx, &Policy::default());
        assert_eq!(out.text, text, "class N is off until the policy turns it on");
    }

    #[test]
    fn a_matched_family_with_the_switch_on_reduces_and_reports_lossy() {
        let text = cargo_test_output();
        let ctx = Ctx { command: Some("cargo test"), path_exists: None };
        let out = reduce(&text, &ctx, &all_on());
        assert_eq!(out.family, Family::Test);
        assert_eq!(out.fired, vec!["tests"]);
        assert!(out.lossy);
        assert!(out.text.contains("test result: ok. 60 passed"));
        assert!(out.text.len() < text.len() / 3);
    }

    #[test]
    fn the_accept_gate_rejects_a_small_win() {
        // One folded include chain in a large build log saves far less than 10%.
        let mut text = String::from("In file included from a.h:1,\n                 from b.h:2,\n                 from c.h:3,\n                 from d.h:4,\n                 from main.c:5:\n");
        for i in 0..200 {
            text.push_str(&format!("unique build line number {i} with some words\n"));
        }
        let ctx = Ctx { command: Some("make"), path_exists: None };
        let out = reduce(&text, &ctx, &all_on());
        assert_eq!(out.text, text);
        assert!(out.fired.is_empty());
    }

    #[test]
    fn reducing_twice_is_a_no_op() {
        let text = cargo_test_output();
        let ctx = Ctx { command: Some("cargo test"), path_exists: None };
        let once = reduce(&text, &ctx, &all_on());
        let twice = reduce(&once.text, &ctx, &all_on());
        assert_eq!(twice.text, once.text);
    }

    #[test]
    fn the_brake_passes_everything_through() {
        let text = format!("\u{1b}[31m{}\u{1b}[0m", "x".repeat(2000));
        let mut p = all_on();
        p.brake = true;
        let ctx = Ctx { command: Some("cargo test"), path_exists: None };
        assert_eq!(reduce(&text, &ctx, &p).text, text);
    }

    #[test]
    fn search_grouping_is_lossless_and_on_by_default() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("crates/very/long/path/to/module/file_a.rs:{i}:    let value_{i} = compute();\n"));
        }
        let exists = |_: &str| true;
        let ctx = Ctx { command: Some("rg -n compute"), path_exists: Some(&exists) };
        let out = reduce(&text, &ctx, &Policy::default());
        assert_eq!(out.fired, vec!["search-group"]);
        assert!(!out.lossy);
        assert_eq!(out.text.matches("file_a.rs").count(), 1);
    }
}
