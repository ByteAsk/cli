//! Local only for this ByteAsk home (the trial's network promise).
//!
//! Two halves:
//! * `policy.toml` `[network] local_only = true`: ByteAsk's online tools
//!   (Compiler Explorer, C++ Insights, Quick C++ Benchmark, the intrinsics
//!   lookup) and the reference lookups answer NOT RUN (offline), and the
//!   toolbox never checks the plan online;
//! * `config.toml`: the CLI's own calls home are switched off: the update
//!   check, web search, telemetry and metrics, analytics, the plugin
//!   marketplace and apps (the complete list for company installs, as measured
//!   in a network jail).
//!
//! The model provider is not touched: the agent's model calls go where the
//! engineer configured them. Keys already set to the quiet value are left as
//! they are; everything else in `config.toml` is kept byte for byte.

use std::path::Path;

use check_result::CheckResult;
use check_result::NotRunReason;
use toml_edit::DocumentMut;

/// `(table, key, value)` of every setting written.
const QUIET: [(&str, &str, Quiet); 8] = [
    ("", "check_for_update_on_startup", Quiet::Bool(false)),
    ("", "web_search", Quiet::Str("disabled")),
    ("otel", "exporter", Quiet::Str("none")),
    ("otel", "trace_exporter", Quiet::Str("none")),
    ("otel", "metrics_exporter", Quiet::Str("none")),
    ("analytics", "enabled", Quiet::Bool(false)),
    ("features", "plugins", Quiet::Bool(false)),
    ("features", "apps", Quiet::Bool(false)),
];

#[derive(Clone, Copy, Debug)]
enum Quiet {
    Bool(bool),
    Str(&'static str),
}

impl Quiet {
    fn item(self) -> toml_edit::Item {
        match self {
            Quiet::Bool(b) => toml_edit::value(b),
            Quiet::Str(s) => toml_edit::value(s),
        }
    }

    fn matches(self, item: &toml_edit::Item) -> bool {
        match self {
            Quiet::Bool(b) => item.as_bool() == Some(b),
            Quiet::Str(s) => item.as_str() == Some(s),
        }
    }
}

/// Set the quiet keys in `text` (a `config.toml`). Returns the new text and
/// the keys that changed (`table.key`).
pub fn quiet_config(text: &str) -> Result<(String, Vec<String>), String> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|err| format!("config.toml could not be read: {err}"))?;
    let mut changed = Vec::new();
    for (table, key, value) in QUIET {
        let name = if table.is_empty() {
            key.to_string()
        } else {
            format!("{table}.{key}")
        };
        if table.is_empty() {
            if doc.get(key).is_some_and(|item| value.matches(item)) {
                continue;
            }
            doc[key] = value.item();
        } else {
            if !doc.contains_key(table) {
                doc[table] = toml_edit::table();
            }
            let Some(section) = doc[table].as_table_like_mut() else {
                return Err(format!(
                    "config.toml has `{table}` set to something that is not a table"
                ));
            };
            if section.get(key).is_some_and(|item| value.matches(item)) {
                continue;
            }
            section.insert(key, value.item());
        }
        changed.push(name);
    }
    Ok((doc.to_string(), changed))
}

/// Switch this home to local only. PASS names what changed.
pub fn apply(home: &Path) -> CheckResult {
    const NAME: &str = "local only";
    if let Err(err) = byteask_policy::set_local_only(home, true) {
        return CheckResult::not_run(
            NAME,
            NotRunReason::ToolError,
            &format!("could not write the policy file: {err}"),
            "fix the permissions of your ByteAsk home, then run the trial again",
        );
    }
    let (on, problems) = byteask_policy::local_only(home);
    if !on {
        return CheckResult::not_run(
            NAME,
            NotRunReason::ToolError,
            "the policy file did not keep local only on",
            "run `byteask policy show` to see why, then run the trial again",
        )
        .with_evidence(problems);
    }
    let file = home.join("config.toml");
    let before = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return CheckResult::not_run(
                NAME,
                NotRunReason::ToolError,
                &format!("could not read {}: {err}", file.display()),
                "fix the permissions of config.toml, then run the trial again",
            );
        }
    };
    let (after, changed) = match quiet_config(&before) {
        Ok(done) => done,
        Err(problem) => {
            return CheckResult::not_run(
                NAME,
                NotRunReason::ToolError,
                &problem,
                "fix config.toml in your ByteAsk home, then run the trial again",
            );
        }
    };
    if !changed.is_empty() {
        let tmp = home.join("config.toml.trial-tmp");
        // config.toml holds the user's sign-in token: the rewritten file keeps the original's
        // permissions (0600 when there was no file), never the umask default.
        let written = std::fs::write(&tmp, &after)
            .and_then(|()| keep_private_mode(&file, &tmp))
            .and_then(|()| std::fs::rename(&tmp, &file));
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return CheckResult::not_run(
                NAME,
                NotRunReason::ToolError,
                &format!("could not write {}: {err}", file.display()),
                "fix the permissions of config.toml, then run the trial again",
            );
        }
    }
    let what = if changed.is_empty() {
        "already set".to_string()
    } else {
        format!("set {}", changed.join(", "))
    };
    CheckResult::pass(
        NAME,
        &format!(
            "ByteAsk's online tools and reference lookups answer NOT RUN and send nothing; \
             the CLI's update check, web search, telemetry and plugins are off ({what}); \
             only your agent's model calls leave this machine"
        ),
    )
}

/// Give `tmp` the permissions of `original`, or owner-only when `original` does not exist.
fn keep_private_mode(original: &std::path::Path, tmp: &std::path::Path) -> std::io::Result<()> {
    match std::fs::metadata(original) {
        Ok(meta) => std::fs::set_permissions(tmp, meta.permissions()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))
            }
            #[cfg(not(unix))]
            {
                Ok(())
            }
        }
        Err(err) => Err(err),
    }
}

#[cfg(all(test, unix))]
mod private_mode_tests {
    use super::keep_private_mode;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_rewritten_config_keeps_the_original_owner_only_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("config.toml");
        std::fs::write(&original, "token = \"x\"\n").expect("write");
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let tmp = dir.path().join("config.toml.trial-tmp");
        std::fs::write(&tmp, "token = \"x\"\n").expect("write tmp");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).expect("chmod tmp");
        keep_private_mode(&original, &tmp).expect("keep mode");
        assert_eq!(mode(&tmp), 0o600);
    }

    #[test]
    fn a_new_config_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp = dir.path().join("config.toml.trial-tmp");
        std::fs::write(&tmp, "").expect("write tmp");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).expect("chmod tmp");
        keep_private_mode(&dir.path().join("config.toml"), &tmp).expect("keep mode");
        assert_eq!(mode(&tmp), 0o600);
    }
}
