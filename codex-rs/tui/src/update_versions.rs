#[cfg(any(not(debug_assertions), test))]
pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => Some(l > c),
        _ => None,
    }
}

/// The update feed ByteAsk serves at `code.byteask.ai/version.json`:
/// `{"latest":"X.Y.Z","min_supported":"X.Y.Z","notes_url":"…"}`. Every field is
/// optional and unknown fields are ignored, so the feed can grow without breaking
/// installed engines.
#[cfg(any(not(debug_assertions), test))]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct VersionFeed {
    #[serde(default)]
    pub(crate) latest: Option<String>,
    #[serde(default)]
    pub(crate) min_supported: Option<String>,
    #[serde(default)]
    pub(crate) notes_url: Option<String>,
}

/// What startup should do about updates. Never both a prompt and a notice.
#[cfg(any(not(debug_assertions), test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateDecision {
    None,
    /// Newer release exists: a one-line in-session notice, never a blocking screen.
    Notice(String),
    /// Installed version is below `min_supported`: the blocking update prompt.
    Required(String),
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn parse_version_feed(body: &str) -> Option<VersionFeed> {
    serde_json::from_str::<VersionFeed>(body).ok()
}

/// Decide from the installed release version (`None` when unknown) and the feed.
/// Anything unparseable fails open to [`UpdateDecision::None`].
#[cfg(any(not(debug_assertions), test))]
pub(crate) fn update_decision(current: Option<&str>, feed: &VersionFeed) -> UpdateDecision {
    let Some(current) =
        current.filter(|version| parse_version(version).is_some_and(|v| v != (0, 0, 0)))
    else {
        return UpdateDecision::None;
    };
    let newer_latest = feed
        .latest
        .as_deref()
        .filter(|latest| is_newer(latest, current) == Some(true));
    if let Some(min) = feed.min_supported.as_deref()
        && is_newer(min, current) == Some(true)
    {
        return UpdateDecision::Required(newer_latest.unwrap_or(min).trim().to_string());
    }
    match newer_latest {
        Some(latest) => UpdateDecision::Notice(latest.trim().to_string()),
        None => UpdateDecision::None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerVersionNoticeKind {
    Older,
    Different,
}

/// Stable clients compare release precedence. Prerelease clients only order versions
/// within the same release line; local builds and other release lines compare identity.
pub(crate) fn server_version_notice_kind(
    client: &str,
    server: &str,
) -> Option<ServerVersionNoticeKind> {
    let client = semver::Version::parse(client).ok()?;
    let server = semver::Version::parse(server).ok()?;
    let client_release = (client.major, client.minor, client.patch);
    let server_release = (server.major, server.minor, server.patch);
    let client_is_local = client_release == (0, 0, 0) || !client.build.is_empty();
    if client_is_local || (!client.pre.is_empty() && client_release != server_release) {
        return (client != server).then_some(ServerVersionNoticeKind::Different);
    }
    (server.build.is_empty() && server_release != (0, 0, 0) && client > server)
        .then_some(ServerVersionNoticeKind::Older)
}

#[cfg(any(not(debug_assertions), test))]
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut iter = v.trim().split('.');
    let maj = iter.next()?.parse::<u64>().ok()?;
    let min = iter.next()?.parse::<u64>().ok()?;
    let pat = iter.next()?.parse::<u64>().ok()?;
    Some((maj, min, pat))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    fn feed(latest: Option<&str>, min_supported: Option<&str>) -> VersionFeed {
        VersionFeed {
            latest: latest.map(str::to_string),
            min_supported: min_supported.map(str::to_string),
            notes_url: None,
        }
    }

    #[test]
    fn below_min_supported_requires_the_blocking_prompt() {
        let f = feed(Some("0.1.13"), Some("0.1.12"));
        assert_eq!(
            update_decision(Some("0.1.11"), &f),
            UpdateDecision::Required("0.1.13".to_string())
        );
    }

    #[test]
    fn between_min_and_latest_is_only_a_notice() {
        let f = feed(Some("0.1.13"), Some("0.1.10"));
        assert_eq!(
            update_decision(Some("0.1.12"), &f),
            UpdateDecision::Notice("0.1.13".to_string())
        );
    }

    #[test]
    fn equal_or_newer_local_version_says_nothing() {
        let f = feed(Some("0.1.13"), Some("0.1.13"));
        assert_eq!(update_decision(Some("0.1.13"), &f), UpdateDecision::None);
        assert_eq!(update_decision(Some("0.2.0"), &f), UpdateDecision::None);
    }

    #[test]
    fn missing_min_supported_never_blocks() {
        let f = feed(Some("0.1.13"), None);
        assert_eq!(
            update_decision(Some("0.0.9"), &f),
            UpdateDecision::Notice("0.1.13".to_string())
        );
    }

    #[test]
    fn unknown_or_source_build_version_skips_every_check() {
        let f = feed(Some("9.9.9"), Some("9.9.9"));
        assert_eq!(update_decision(None, &f), UpdateDecision::None);
        assert_eq!(update_decision(Some("0.0.0"), &f), UpdateDecision::None);
        assert_eq!(update_decision(Some("dev"), &f), UpdateDecision::None);
    }

    #[test]
    fn unparseable_feed_versions_fail_open() {
        assert_eq!(
            update_decision(Some("0.1.12"), &feed(Some("soon"), Some("later"))),
            UpdateDecision::None
        );
        assert_eq!(
            update_decision(Some("0.1.12"), &feed(None, None)),
            UpdateDecision::None
        );
    }

    #[test]
    fn feed_parsing_tolerates_extra_and_missing_fields_and_rejects_garbage() {
        assert_eq!(
            parse_version_feed(r#"{"latest":"0.1.13","severity":"critical"}"#),
            Some(feed(Some("0.1.13"), None))
        );
        assert_eq!(
            parse_version_feed(
                r#"{"latest":"0.1.13","min_supported":"0.1.10","notes_url":"https://x"}"#
            )
            .map(|f| f.min_supported),
            Some(Some("0.1.10".to_string()))
        );
        assert_eq!(parse_version_feed("not json"), None);
        assert_eq!(parse_version_feed("[1,2]"), None);
        assert_eq!(parse_version_feed(""), None);
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(parse_version(" 1.2.3 \n"), Some((1, 2, 3)));
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }

    #[test]
    fn stable_clients_only_warn_for_older_releases() {
        for (client, server, expected) in [
            ("0.152.1", "0.152.0", Some(ServerVersionNoticeKind::Older)),
            ("0.153.0", "0.152.1", Some(ServerVersionNoticeKind::Older)),
            (
                "0.153.0",
                "0.153.0-alpha.10.1",
                Some(ServerVersionNoticeKind::Older),
            ),
            (
                "0.156.0",
                "0.155.0-alpha.12",
                Some(ServerVersionNoticeKind::Older),
            ),
            ("0.153.0", "0.153.0", None),
            ("0.153.0", "0.154.0", None),
            ("0.153.0", "0.154.0-alpha.1", None),
            ("0.153.0", "0.0.0", None),
            ("0.153.0", "0.0.0-alpha.1", None),
            ("0.153.0", "0.152.0+dev", None),
        ] {
            assert_eq!(server_version_notice_kind(client, server), expected);
        }
    }

    #[test]
    fn prerelease_clients_compare_within_the_same_release_line() {
        for (newer, older) in [
            ("0.155.0-alpha.23", "0.155.0-alpha.22"),
            ("0.155.0-alpha.24", "0.155.0-alpha.23"),
            ("0.153.0-alpha.10", "0.153.0-alpha.9"),
            ("0.153.0-alpha.10.1", "0.153.0-alpha.9.2"),
            ("0.153.0-alpha.10.10", "0.153.0-alpha.10.9"),
            ("0.153.0-alpha.10.1", "0.153.0-alpha.10"),
            ("0.155.0", "0.155.0-alpha.23"),
        ] {
            assert_eq!(
                server_version_notice_kind(newer, older),
                Some(ServerVersionNoticeKind::Older)
            );
            assert_eq!(server_version_notice_kind(older, newer), None);
            assert_eq!(server_version_notice_kind(newer, newer), None);
        }
    }

    #[test]
    fn prerelease_clients_on_other_release_lines_and_local_clients_warn_for_mismatches() {
        for (client, server) in [
            ("0.155.0-alpha.23", "0.156.0"),
            ("0.155.0-alpha.12", "0.154.0"),
            ("0.0.0", "0.153.0"),
            ("0.0.0", "0.153.0-alpha.10"),
            ("0.153.0+dev", "0.153.0"),
            ("0.153.0-alpha.10", "0.0.0"),
        ] {
            assert_eq!(
                server_version_notice_kind(client, server),
                Some(ServerVersionNoticeKind::Different)
            );
            assert_eq!(server_version_notice_kind(client, client), None);
        }
    }

    #[test]
    fn unknown_or_malformed_versions_do_not_produce_notices() {
        for version in [
            "unknown",
            "dev",
            "0.0.0.0",
            "0.153",
            "0.153.0.1",
            " 0.153.0",
            "+0.153.0",
            "0.0153.0",
            "0.153.0-alpha.01",
            "0.153.0-alpha..1",
        ] {
            for release in ["0.153.0", "0.153.0-alpha.10", "0.0.0"] {
                assert_eq!(server_version_notice_kind(version, release), None);
                assert_eq!(server_version_notice_kind(release, version), None);
            }
        }
    }
}
