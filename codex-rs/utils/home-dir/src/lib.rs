use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::PathBuf;

/// Returns the ByteAsk configuration directory.
///
/// `BYTEASK_HOME` wins; `CODEX_HOME` is still honored as a silent fallback so
/// existing setups keep working; with neither set it defaults to `~/.byteask`.
///
/// - If a home variable is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If neither is set, this function does not verify that the directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let env = |name: &str| std::env::var(name).ok().filter(|val| !val.is_empty());
    find_codex_home_from_env(env("BYTEASK_HOME").as_deref(), env("CODEX_HOME").as_deref())
}

fn find_codex_home_from_env(
    byteask_home_env: Option<&str>,
    codex_home_env: Option<&str>,
) -> std::io::Result<AbsolutePathBuf> {
    // Errors always name BYTEASK_HOME: it is the variable ByteAsk documents, and the
    // legacy name is only read so older setups do not break.
    match byteask_home_env.or(codex_home_env) {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("BYTEASK_HOME points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read BYTEASK_HOME {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("BYTEASK_HOME points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize BYTEASK_HOME {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".byteask");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err =
            find_codex_home_from_env(Some(missing_str), None).expect_err("missing BYTEASK_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("BYTEASK_HOME") && !err.to_string().contains("CODEX"),
            "unexpected error: {err}"
        );
    }

    /// ⚠️ Compatibility marker, not just copy: `byteask/cli/byteask` (`_engine_compat`)
    /// and `byteask/cli/byteask.ps1` (`Test-EngineHasMarker`) grep the engine binary for
    /// the exact text `BYTEASK_HOME points to ` (trailing space included) to decide
    /// whether it reads BYTEASK_HOME. Changing this wording makes new wrappers treat a
    /// new engine as a legacy one.
    #[test]
    fn missing_home_error_keeps_the_wrapper_compat_marker() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing");
        let missing_str = missing.to_str().expect("utf-8");

        for (byteask, codex) in [(Some(missing_str), None), (None, Some(missing_str))] {
            let err = find_codex_home_from_env(byteask, codex).expect_err("missing home");
            assert!(
                err.to_string().contains("BYTEASK_HOME points to "),
                "wrapper compat marker missing: {err}"
            );
        }
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env(None, Some(file_str)).expect_err("file CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str), None).expect("valid BYTEASK_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved = find_codex_home_from_env(/*byteask_home_env*/ None, /*codex_home_env*/ None)
            .expect("default home");
        let mut expected = home_dir().expect("home dir");
        expected.push(".byteask");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn byteask_home_wins_and_codex_home_is_a_silent_fallback() {
        let byteask = TempDir::new().expect("byteask home");
        let codex = TempDir::new().expect("codex home");
        let b = byteask.path().to_str().expect("utf-8");
        let c = codex.path().to_str().expect("utf-8");
        let canon = |p: &std::path::Path| {
            AbsolutePathBuf::from_absolute_path(p.canonicalize().expect("canonicalize"))
                .expect("absolute")
        };

        let both = find_codex_home_from_env(Some(b), Some(c)).expect("both set");
        assert_eq!(both, canon(byteask.path()));

        let fallback = find_codex_home_from_env(None, Some(c)).expect("fallback");
        assert_eq!(fallback, canon(codex.path()));
    }
}
