//! `[network]` in the policy file: whether ByteAsk's own tools may reach the
//! network.
//!
//! ```toml
//! [network]
//! local_only = true   # ByteAsk's online tools and reference lookups answer NOT RUN (offline)
//! ```
//!
//! Set by `byteask company trial`, so an engineer can try ByteAsk before their
//! security review with nothing leaving the laptop except the agent's own
//! model calls. Unlike the other settings, the safe default of a damaged file
//! is the restrictive one: a file that exists but cannot be trusted or read
//! means local only (it can only stop network use, never start it). No file,
//! or no `[network]` section, means the normal product (network allowed).

use std::path::Path;

/// The fix every local-only NOT RUN carries (the exact command).
pub const LOCAL_ONLY_FIX: &str = "answer without this tool and say so; only the user can allow it again, by \
running `byteask policy local-only off` in a terminal";

/// The summary of a tool that did not run because the machine is local-only.
pub fn local_only_summary(tool: &str, destination: &str) -> String {
    format!(
        "{tool} did not run: this machine is set to local only, and {tool} would send your \
         question to {destination}"
    )
}

/// Is this ByteAsk home local-only? Never fails; problems are returned in
/// plain words beside the answer.
pub fn local_only(home: &Path) -> (bool, Vec<String>) {
    let text = match super::read_trusted(&super::path(home)) {
        Ok(Some(text)) => text,
        Ok(None) => return (false, Vec::new()),
        Err(problem) => {
            return (
                true,
                vec![format!(
                    "{problem} ByteAsk's own tools stay off the network until it is fixed."
                )],
            );
        }
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return (
            true,
            vec![
                "the policy file could not be read, so ByteAsk's own tools stay off the network \
                 until it is fixed."
                    .to_string(),
            ],
        );
    };
    let Some(section) = doc.get("network") else {
        return (false, Vec::new());
    };
    match section.as_table().and_then(|table| table.get("local_only")) {
        None => (false, Vec::new()),
        Some(toml::Value::Boolean(on)) => (*on, Vec::new()),
        Some(_) => (
            true,
            vec!["network.local_only must be true or false; treating it as true.".to_string()],
        ),
    }
}

/// Turn local-only on or off (the one writer, like every policy setting).
pub fn set_local_only(home: &Path, on: bool) -> std::io::Result<()> {
    super::update(home, |doc| {
        if !doc.contains_table("network") {
            doc["network"] = toml_edit::table();
        }
        doc["network"]["local_only"] = toml_edit::value(on);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn no_file_or_no_section_is_the_normal_product() {
        let home = tempfile::tempdir().expect("home");
        assert_eq!(local_only(home.path()), (false, Vec::new()));
        super::super::update(home.path(), |doc| {
            doc["results_store"] = toml_edit::table();
            doc["results_store"]["max_days"] = toml_edit::value(30);
        })
        .expect("write");
        assert_eq!(local_only(home.path()), (false, Vec::new()));
    }

    #[test]
    fn set_reads_back_and_keeps_other_sections() {
        let home = tempfile::tempdir().expect("home");
        super::super::update(home.path(), |doc| {
            doc["results_store"] = toml_edit::table();
            doc["results_store"]["max_days"] = toml_edit::value(30);
        })
        .expect("write");
        set_local_only(home.path(), true).expect("set");
        assert_eq!(local_only(home.path()), (true, Vec::new()));
        let (limits, _) = super::super::results_store_limits(home.path());
        assert_eq!(limits.max_days, 30, "other sections are kept");
        set_local_only(home.path(), false).expect("unset");
        assert_eq!(local_only(home.path()).0, false);
    }

    /// A damaged or untrusted file can only keep the tools off the network.
    #[test]
    fn a_file_that_cannot_be_trusted_or_read_means_local_only() {
        let home = tempfile::tempdir().expect("home");
        std::fs::write(super::super::path(home.path()), "this is [not toml").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                super::super::path(home.path()),
                std::fs::Permissions::from_mode(0o600),
            )
            .expect("chmod");
        }
        let (on, problems) = local_only(home.path());
        assert!(on);
        assert_eq!(problems.len(), 1);
        std::fs::write(
            super::super::path(home.path()),
            "[network]\nlocal_only = \"yes\"\n",
        )
        .expect("write");
        assert!(local_only(home.path()).0, "a value that is not a boolean");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(
                super::super::path(home.path()),
                "[network]\nlocal_only = false\n",
            )
            .expect("write");
            std::fs::set_permissions(
                super::super::path(home.path()),
                std::fs::Permissions::from_mode(0o666),
            )
            .expect("chmod");
            assert!(
                local_only(home.path()).0,
                "a file others can write cannot switch local-only off"
            );
        }
    }
}
