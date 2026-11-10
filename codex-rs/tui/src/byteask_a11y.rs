//! ByteAsk accessibility switches.
//!
//! Three questions every ByteAsk surface asks before drawing:
//! - [`screen_reader_mode`]: render linear, announce-once text instead of in-place repaints
//!   (Claude Code's `--ax-screen-reader` shape). Flag > env > config, like Claude Code.
//! - [`ascii_glyphs`]: substitute the glyph set when the locale is not UTF-8 or in
//!   screen-reader mode.
//! - [`glyph`]: the one place the `✓ ✗ › ▌ █ ░ ·` vocabulary is mapped to ASCII.
//!
//! Colour is never the only channel — that is a property of the copy, not a
//! switch here; `NO_COLOR` is honoured by upstream's colour-level degrade.
use std::sync::OnceLock;

/// Set by the CLI when `--ax-screen-reader` is passed, before the TUI starts.
static FLAG: OnceLock<bool> = OnceLock::new();
/// Set from `tui.screen_reader` in config, before the TUI starts.
static CONFIG: OnceLock<bool> = OnceLock::new();

pub(crate) fn set_flag(on: bool) {
    let _ = FLAG.set(on);
}

pub(crate) fn set_config(on: bool) {
    let _ = CONFIG.set(on);
}

/// `None` means "not set"; `Some(false)` means "set, and deliberately off" — the two are not
/// interchangeable, because an explicit off vetoes the config (see [`resolve_source`]).
fn parse_truthy(raw: &str) -> Option<bool> {
    let v = raw.trim();
    if v.is_empty() {
        return None;
    }
    Some(v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

fn env_truthy(name: &str) -> Option<bool> {
    parse_truthy(&std::env::var(name).ok()?)
}

/// The documented switch (`docs/config.md`).
const ENV_SCREEN_READER: &str = "BYTEASK_AX_SCREEN_READER";
/// The wrapper's older switch, kept as an alias so an existing `_scr_*` user is not stranded.
const ENV_PLAIN_MENU: &str = "BYTEASK_PLAIN_MENU";

/// Where the mode came from, for the confirmation line
/// (`[Screen reader mode: on via env]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenReaderSource {
    Flag,
    Env,
    Config,
}

/// The precedence rule, as a pure function of the four inputs.
///
/// Split out from [`screen_reader_source`] so the matrix can be tested without writing the
/// process-global `OnceLock`s (write-once — a test that set them would change every later
/// test in the same binary) or the process environment (shared by every thread in a
/// `cargo test` run).
fn resolve_source(
    flag: bool,
    ax_env: Option<bool>,
    plain_menu_env: Option<bool>,
    config: bool,
) -> Option<ScreenReaderSource> {
    if flag {
        return Some(ScreenReaderSource::Flag);
    }
    // An explicit `BYTEASK_AX_SCREEN_READER=0` is a decision, not an absence: it keeps the
    // mode off even when the config says on, and it is why this is a three-way match.
    match ax_env {
        Some(true) => return Some(ScreenReaderSource::Env),
        Some(false) => return None,
        None => {}
    }
    if plain_menu_env == Some(true) {
        return Some(ScreenReaderSource::Env);
    }
    if config {
        return Some(ScreenReaderSource::Config);
    }
    None
}

/// Flag beats env beats config; `BYTEASK_AX_SCREEN_READER=0` keeps the mode off even when
/// the config says on (Claude Code's precedence). `BYTEASK_PLAIN_MENU=1` (the wrapper's older
/// switch) is an alias for the env form.
pub(crate) fn screen_reader_source() -> Option<ScreenReaderSource> {
    resolve_source(
        FLAG.get().copied().unwrap_or(false),
        env_truthy(ENV_SCREEN_READER),
        env_truthy(ENV_PLAIN_MENU),
        CONFIG.get().copied().unwrap_or(false),
    )
}

pub(crate) fn screen_reader_mode() -> bool {
    screen_reader_source().is_some()
}

/// The confirmation line printed first in screen-reader mode.
pub(crate) fn confirmation_line() -> Option<String> {
    screen_reader_source().map(line_for_source)
}

/// The wording, separated from the lookup so it can be tested against a chosen source
/// without writing the process-global state [`screen_reader_source`] reads.
fn line_for_source(source: ScreenReaderSource) -> String {
    let via = match source {
        ScreenReaderSource::Flag => "flag",
        ScreenReaderSource::Env => "env",
        ScreenReaderSource::Config => "settings",
    };
    format!("[Screen reader mode: on via {via}]")
}

/// Write the confirmation line, if the mode is on, before anything else the process prints.
///
/// Called exactly once, from `run_main`, while stdout is still a plain stream — after that the
/// terminal is in raw mode and may be on the alternate screen, where this line would be
/// swallowed. Only [`ScreenReaderSource::Flag`] and [`ScreenReaderSource::Env`] can be known
/// this early; `tui.screen_reader` is applied after config load and turns the mode on without
/// announcing itself.
#[allow(clippy::print_stdout)]
pub(crate) fn print_confirmation_line() {
    if let Some(line) = confirmation_line() {
        println!("{line}");
    }
}

/// Pure half of [`locale_is_utf8`]: the first of `LC_ALL`, `LC_CTYPE`, `LANG` that is set and
/// non-empty decides, exactly as POSIX resolves the locale.
fn locale_is_utf8_from(lookup: impl Fn(&str) -> Option<String>) -> bool {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(v) = lookup(name) {
            if !v.is_empty() {
                let v = v.to_ascii_lowercase();
                return v.contains("utf-8") || v.contains("utf8");
            }
        }
    }
    // No locale variables at all (Windows, some CI): assume a modern terminal.
    true
}

fn locale_is_utf8() -> bool {
    locale_is_utf8_from(|name| std::env::var(name).ok())
}

/// ASCII glyphs when the terminal cannot show the Unicode set or a screen reader is on.
pub(crate) fn ascii_glyphs() -> bool {
    screen_reader_mode() || !locale_is_utf8()
}

/// The whole ByteAsk glyph vocabulary and its ASCII fallbacks.
pub(crate) fn glyph(unicode: &'static str) -> &'static str {
    glyph_for(unicode, ascii_glyphs())
}

/// Pure half of [`glyph`], so the substitution table is testable without process state.
fn glyph_for(unicode: &'static str, ascii: bool) -> &'static str {
    if !ascii {
        return unicode;
    }
    match unicode {
        "▌" => "|",
        "›" => ">",
        "✓" => "OK",
        "✗" => "x",
        "█" => "#",
        "░" => "-",
        "·" => "-",
        "…" => "...",
        "—" => "-",
        "→" => "->",
        "×" => "x",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // **No test in this module writes to the process environment, deliberately.**
    //
    // `screen_reader_mode()` and `ascii_glyphs()` are read while panes render, and the
    // `codex-tui` test binary runs several thousand tests as threads in one process -- so
    // setting `BYTEASK_AX_SCREEN_READER` for even two syscalls can flip a concurrent snapshot
    // to its ASCII form. `serial_test::serial` does not help: it serialises marked tests
    // against each other, not against the thousands that are unmarked. `FLAG` and `CONFIG` are
    // worse still -- `OnceLock` is write-once, so a test that set one would change every later
    // test in the binary.
    //
    // The precedence matrix therefore runs against the pure `resolve_source`, the wording
    // against `line_for_source`, the parser against `parse_truthy`, and the variable names are
    // pinned as constants -- together every step except `std::env::var` itself.

    // ---- precedence (flag > env > config) --------------------------------------------

    #[test]
    fn off_when_nothing_asks_for_it() {
        assert_eq!(resolve_source(false, None, None, false), None);
    }

    #[test]
    fn flag_beats_env_and_config() {
        // Even an explicit env "off" loses to the flag the user just typed.
        assert_eq!(
            resolve_source(true, Some(false), None, false),
            Some(ScreenReaderSource::Flag)
        );
        assert_eq!(
            resolve_source(true, Some(true), Some(true), true),
            Some(ScreenReaderSource::Flag)
        );
    }

    #[test]
    fn env_beats_config() {
        assert_eq!(
            resolve_source(false, Some(true), None, false),
            Some(ScreenReaderSource::Env)
        );
        // Same answer with the config also on -- the *source* must still read as env, because
        // that is the word the confirmation line prints.
        assert_eq!(
            resolve_source(false, Some(true), None, true),
            Some(ScreenReaderSource::Env)
        );
    }

    #[test]
    fn env_zero_beats_a_true_config() {
        assert_eq!(resolve_source(false, Some(false), None, true), None);
        // And it wins over the legacy alias too, so one variable turns the mode off outright.
        assert_eq!(resolve_source(false, Some(false), Some(true), true), None);
    }

    #[test]
    fn plain_menu_is_an_alias_for_the_env_form() {
        assert_eq!(
            resolve_source(false, None, Some(true), false),
            Some(ScreenReaderSource::Env)
        );
        // BYTEASK_PLAIN_MENU=0 is not a veto -- only BYTEASK_AX_SCREEN_READER=0 is.
        assert_eq!(
            resolve_source(false, None, Some(false), true),
            Some(ScreenReaderSource::Config)
        );
    }

    #[test]
    fn config_is_the_last_source_consulted() {
        assert_eq!(
            resolve_source(false, None, None, true),
            Some(ScreenReaderSource::Config)
        );
    }

    #[test]
    fn env_truthy_accepts_the_documented_spellings() {
        // Guards the parser feeding `resolve_source`; `env_truthy` returns Some(false) for
        // anything set-but-not-truthy, which is what makes an explicit off distinguishable
        // from an unset variable.
        for on in ["1", "true", "TRUE", "yes", " 1 "] {
            assert_eq!(parse_truthy(on), Some(true), "{on:?} should read as on");
        }
        for off in ["0", "false", "no", "off"] {
            assert_eq!(parse_truthy(off), Some(false), "{off:?} should read as off");
        }
        assert_eq!(parse_truthy(""), None, "an empty value is an absent value");
    }

    /// The names are the published contract: `docs/config.md`, the wrapper's `_scr_*`
    /// screens and every support answer spell them out, so a rename here is a breaking
    /// change and must be a deliberate edit to this test.
    #[test]
    fn the_environment_variable_names_are_the_documented_ones() {
        assert_eq!(ENV_SCREEN_READER, "BYTEASK_AX_SCREEN_READER");
        assert_eq!(ENV_PLAIN_MENU, "BYTEASK_PLAIN_MENU");
    }

    /// Reads the real environment (without writing to it): an unset variable must be `None`,
    /// not `Some(false)`, or an absent variable would veto the config.
    #[test]
    fn an_unset_variable_reads_as_absent() {
        assert_eq!(env_truthy("BYTEASK_AX_SCREEN_READER_UNSET_PROBE"), None);
    }

    // ---- confirmation line ---------------------------------------------------------------

    #[test]
    fn confirmation_line_names_the_source() {
        assert_eq!(
            line_for_source(ScreenReaderSource::Flag),
            "[Screen reader mode: on via flag]"
        );
        assert_eq!(
            line_for_source(ScreenReaderSource::Env),
            "[Screen reader mode: on via env]"
        );
        // `settings`, not `config` -- the user set it in a settings file, and the word has to
        // tell them where to go and turn it off.
        assert_eq!(
            line_for_source(ScreenReaderSource::Config),
            "[Screen reader mode: on via settings]"
        );
    }

    #[test]
    fn there_is_no_line_when_the_mode_is_off() {
        // `confirmation_line` is `screen_reader_source().map(line_for_source)`, and
        // `resolve_source` is pinned above, so this covers the composition: no source, no
        // line, and therefore nothing printed on an ordinary launch.
        assert_eq!(
            resolve_source(false, None, None, false).map(line_for_source),
            None
        );
    }

    /// The wording is a product surface, not an incidental string: it is the very
    /// first thing a screen reader announces.
    #[test]
    fn confirmation_line_is_plain_ascii_sentence_case() {
        for source in [
            ScreenReaderSource::Flag,
            ScreenReaderSource::Env,
            ScreenReaderSource::Config,
        ] {
            let line = line_for_source(source);
            assert!(
                line.is_ascii(),
                "{line:?} must survive a non-UTF-8 terminal"
            );
            assert!(
                !line.contains('!'),
                "{line:?} breaks the no-exclamation rule"
            );
            assert!(line.starts_with("[Screen reader mode: on via "));
            assert!(line.ends_with(']'));
        }
    }

    // ---- glyphs / locale -----------------------------------------------------------------

    #[test]
    fn lang_c_forces_ascii_glyphs() {
        let ascii = !locale_is_utf8_from(|name| (name == "LANG").then(|| "C".to_string()));
        assert!(ascii, "LANG=C is not a UTF-8 locale");
        assert_eq!(glyph_for("✓", ascii), "OK");
        assert_eq!(glyph_for("›", ascii), ">");
        assert_eq!(glyph_for("▌", ascii), "|");
    }

    #[test]
    fn a_utf8_locale_keeps_the_unicode_glyphs() {
        let ascii =
            !locale_is_utf8_from(|name| (name == "LANG").then(|| "en_US.UTF-8".to_string()));
        assert!(!ascii);
        assert_eq!(glyph_for("✓", ascii), "✓");
    }

    #[test]
    fn lc_all_outranks_lang() {
        // POSIX order: LC_ALL, then LC_CTYPE, then LANG -- the first one set decides.
        let lookup = |name: &str| match name {
            "LC_ALL" => Some("C".to_string()),
            "LANG" => Some("en_US.UTF-8".to_string()),
            _ => None,
        };
        assert!(!locale_is_utf8_from(lookup));
    }

    #[test]
    fn an_empty_locale_variable_is_skipped() {
        let lookup = |name: &str| match name {
            "LC_ALL" => Some(String::new()),
            "LANG" => Some("en_US.UTF-8".to_string()),
            _ => None,
        };
        assert!(locale_is_utf8_from(lookup));
    }

    #[test]
    fn no_locale_variables_assume_a_modern_terminal() {
        assert!(locale_is_utf8_from(|_| None));
    }

    #[test]
    fn every_glyph_in_the_vocabulary_has_an_ascii_fallback() {
        // The glyph vocabulary plus the punctuation the panes use.
        for g in ["▌", "›", "✓", "✗", "█", "░", "·", "…", "—", "→", "×"] {
            let ascii = glyph_for(g, true);
            assert!(!ascii.is_empty(), "{g} has an empty fallback");
            assert!(ascii.is_ascii(), "{g} falls back to {ascii:?}, not ASCII");
            assert_eq!(
                glyph_for(g, false),
                g,
                "{g} must be itself when UTF-8 is fine"
            );
        }
    }
}
