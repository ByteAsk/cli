//! Cloud/local provider presets for the "your own hosted model" flow.
//!
//! The preset table is **vendored in exactly one place** —
//! `byteask/gateway/byteask_models.py` — and both shell wrappers already shell
//! out to it rather than keeping their own copies. The TUI does the same: a
//! third copy would drift the moment a provider changes its base URL.
//!
//! Contract (`byteask_models.py presets`), one preset per line:
//! `id|base_url|wire|needs_key|key_env|kind`.

use std::path::Path;
use std::process::Command;

/// Whether a preset runs on someone else's hardware (and bills the user) or on
/// the user's own machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetKind {
    /// A hosted API the user pays for with their own key.
    Cloud,
    /// A server the user runs themselves.
    Local,
}

impl PresetKind {
    /// The right-hand column label. Deliberately blunt about who pays — this is
    /// the one thing a user must not misread when picking a provider.
    pub(crate) fn billing_tag(self) -> &'static str {
        match self {
            PresetKind::Cloud => "they bill you",
            PresetKind::Local => "never billed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderPreset {
    pub id: String,
    pub base_url: String,
    pub needs_key: bool,
    pub kind: PresetKind,
}

/// Minimum `byteask_models.py` version exposing the `presets` subcommand.
/// Mirrors the wrapper's `_preset_helper_available` handshake so a new engine
/// paired with an old helper degrades to "no presets" instead of erroring.
const MIN_HELPER_VERSION: u32 = 2;

fn helper_path(codex_home: &Path) -> std::path::PathBuf {
    codex_home.join("byteask_models.py")
}

/// Load the preset table, cloud entries first then local, each alphabetical.
///
/// Returns an empty list whenever the helper is missing, too old, or fails —
/// the caller then offers only the "custom server" path, which needs no helper.
pub(crate) fn load(codex_home: &Path) -> Vec<ProviderPreset> {
    let helper = helper_path(codex_home);
    if !helper.is_file() || !helper_is_current(&helper) {
        return Vec::new();
    }
    let Some(stdout) = run_helper(&helper, "presets") else {
        return Vec::new();
    };
    let mut presets = parse(&stdout);
    // Cloud first (the common case), local after; alphabetical within each group.
    presets.sort_by(|a, b| match (a.kind, b.kind) {
        (PresetKind::Cloud, PresetKind::Local) => std::cmp::Ordering::Less,
        (PresetKind::Local, PresetKind::Cloud) => std::cmp::Ordering::Greater,
        _ => a.id.cmp(&b.id),
    });
    presets
}

fn helper_is_current(helper: &Path) -> bool {
    run_helper(helper, "version")
        .and_then(|out| out.trim().parse::<u32>().ok())
        .is_some_and(|version| version >= MIN_HELPER_VERSION)
}

fn run_helper(helper: &Path, subcommand: &str) -> Option<String> {
    let output = Command::new("python3")
        .arg(helper)
        .arg(subcommand)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Parse `id|base_url|wire|needs_key|key_env|kind` lines, skipping malformed ones.
fn parse(stdout: &str) -> Vec<ProviderPreset> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim().split('|');
            let id = fields.next()?.trim();
            let base_url = fields.next()?.trim();
            let _wire = fields.next()?;
            let needs_key = fields.next()?.trim() == "1";
            let _key_env = fields.next()?;
            let kind = match fields.next()?.trim() {
                "cloud" => PresetKind::Cloud,
                "local" => PresetKind::Local,
                _ => return None,
            };
            if id.is_empty() || base_url.is_empty() {
                return None;
            }
            Some(ProviderPreset {
                id: id.to_string(),
                base_url: base_url.to_string(),
                needs_key,
                kind,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SAMPLE: &str = "\
groq|https://api.groq.com/openai/v1|chat|1|GROQ_API_KEY|cloud
ollama|http://localhost:11434|auto|0||local
";

    #[test]
    fn parses_the_helper_contract() {
        let presets = parse(SAMPLE);
        assert_eq!(
            presets,
            vec![
                ProviderPreset {
                    id: "groq".to_string(),
                    base_url: "https://api.groq.com/openai/v1".to_string(),
                    needs_key: true,
                    kind: PresetKind::Cloud,
                },
                ProviderPreset {
                    id: "ollama".to_string(),
                    base_url: "http://localhost:11434".to_string(),
                    needs_key: false,
                    kind: PresetKind::Local,
                },
            ]
        );
    }

    #[test]
    fn skips_malformed_lines_without_dropping_good_ones() {
        let stdout = format!("not-a-preset\n\n|https://x|chat|1|E|cloud\n{SAMPLE}");
        let presets = parse(&stdout);
        assert_eq!(
            presets.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["groq", "ollama"]
        );
    }

    #[test]
    fn unknown_kind_is_rejected_rather_than_guessed() {
        // Billing is the whole point of `kind`; guessing it could tell a user
        // "never billed" about a paid endpoint.
        assert!(parse("mystery|https://x|chat|1|E|somethingelse").is_empty());
    }

    #[test]
    fn missing_helper_yields_no_presets() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn billing_tags_are_unambiguous() {
        assert_eq!(PresetKind::Cloud.billing_tag(), "they bill you");
        assert_eq!(PresetKind::Local.billing_tag(), "never billed");
    }
}
