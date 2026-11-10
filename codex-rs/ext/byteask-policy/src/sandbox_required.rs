//! `[sandbox] required` in the policy file: whether ByteAsk's C/C++ tools must run inside their
//! own sandbox (0.2.0).
//!
//! ```toml
//! [sandbox]
//! required = true   # no sandbox available = the tool does not run (NOT RUN, sandbox unavailable)
//! ```
//!
//! Without it, a machine where no sandbox can start (Docker, old kernels, WSL) runs the tools
//! without one and says so in each answer. Like `local_only`, the safe default of a damaged
//! file is the restrictive one: a file that exists but cannot be trusted or read means
//! required. No file, or no `required` key, means not required.

use std::path::Path;

/// Does this ByteAsk home require the tools' sandbox? Never fails; problems are returned in
/// plain words beside the answer.
pub fn sandbox_required(home: &Path) -> (bool, Vec<String>) {
    let text = match super::read_trusted(&super::path(home)) {
        Ok(Some(text)) => text,
        Ok(None) => return (false, Vec::new()),
        Err(problem) => {
            return (
                true,
                vec![format!(
                    "{problem} ByteAsk's tools run only inside their sandbox until it is fixed."
                )],
            );
        }
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return (
            true,
            vec![
                "the policy file could not be read, so ByteAsk's tools run only inside their \
                 sandbox until it is fixed."
                    .to_string(),
            ],
        );
    };
    match doc
        .get("sandbox")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("required"))
    {
        None => (false, Vec::new()),
        Some(toml::Value::Boolean(on)) => (*on, Vec::new()),
        Some(_) => (
            true,
            vec!["sandbox.required must be true or false; treating it as true.".to_string()],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn write(home: &Path, text: &str) {
        std::fs::write(super::super::path(home), text).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                super::super::path(home),
                std::fs::Permissions::from_mode(0o600),
            )
            .expect("chmod");
        }
    }

    #[test]
    fn absent_means_not_required_and_the_key_is_read() {
        let home = tempfile::tempdir().expect("home");
        assert_eq!(sandbox_required(home.path()), (false, Vec::new()));
        write(home.path(), "[sandbox]\nrelaxed_projects = []\n");
        assert_eq!(sandbox_required(home.path()), (false, Vec::new()));
        write(home.path(), "[sandbox]\nrequired = true\n");
        assert_eq!(sandbox_required(home.path()), (true, Vec::new()));
        write(home.path(), "[sandbox]\nrequired = false\n");
        assert_eq!(sandbox_required(home.path()), (false, Vec::new()));
    }

    #[test]
    fn a_damaged_file_or_value_means_required() {
        let home = tempfile::tempdir().expect("home");
        write(home.path(), "this is [not toml");
        assert!(sandbox_required(home.path()).0);
        write(home.path(), "[sandbox]\nrequired = \"yes\"\n");
        assert!(sandbox_required(home.path()).0);
    }
}
