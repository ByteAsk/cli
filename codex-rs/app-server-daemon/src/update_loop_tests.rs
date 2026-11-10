use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::INSTALL_URL;
use super::InstallerHttp;
use super::InstallerResponse;
use super::fetch_installer_script;
use super::update_modes_for_identities;
use crate::RestartMode;
use crate::UpdaterRefreshMode;
use crate::managed_install::executable_identity_from_bytes;

#[test]
fn unchanged_updater_uses_version_based_restart() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"same"),
            &executable_identity_from_bytes(b"same"),
        ),
        (RestartMode::IfVersionChanged, UpdaterRefreshMode::None)
    );
}

#[test]
fn changed_updater_forces_refresh_even_when_version_may_match() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"old"),
            &executable_identity_from_bytes(b"new"),
        ),
        (
            RestartMode::Always,
            UpdaterRefreshMode::ReexecIfManagedBinaryChanged,
        )
    );
}

#[tokio::test]
async fn the_background_daemon_never_self_installs() {
    // Upstream's loop downloaded and RAN an installer on an hourly timer, and the URL
    // it used installs OpenAI's Codex - over a ByteAsk install, while the surrounding
    // error text said "standalone ByteAsk updater". ByteAsk has exactly one update
    // path, `byteask --update`, so this refuses instead and says so. The fake HTTP
    // client proves the refusal happens WITHOUT a request: a test that only checked the
    // error string would still pass if we fetched first and threw the body away.
    let http = FakeInstallerHttp::new(InstallerResponse::Success(b"#!/bin/sh\n".to_vec()));

    let error = fetch_installer_script(&http)
        .await
        .expect_err("the daemon must not self-install");

    assert!(
        error.to_string().contains("byteask --update"),
        "the refusal must name the one update path, got: {error}"
    );
    assert!(
        http.requested_urls().is_empty(),
        "no installer may be fetched at all, requested: {:?}",
        http.requested_urls()
    );
}

#[test]
fn the_daemon_installer_url_is_ours() {
    // Belt and braces: if a future change re-enables the fetch, it must not point at
    // another vendor's installer.
    assert!(
        INSTALL_URL.contains("byteask.ai"),
        "daemon installer URL is not ByteAsk's: {INSTALL_URL}"
    );
}

struct FakeInstallerHttp {
    response: InstallerResponse,
    requested_urls: Mutex<Vec<String>>,
}

impl FakeInstallerHttp {
    fn new(response: InstallerResponse) -> Self {
        Self {
            response,
            requested_urls: Mutex::new(Vec::new()),
        }
    }

    fn requested_urls(&self) -> Vec<String> {
        self.requested_urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl InstallerHttp for FakeInstallerHttp {
    async fn get(&self, url: &str) -> anyhow::Result<InstallerResponse> {
        self.requested_urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(url.to_string());
        Ok(self.response.clone())
    }
}
