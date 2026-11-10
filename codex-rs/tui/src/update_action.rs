#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallContext;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallMethod;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::StandalonePlatform;

/// Detected installation method. Upstream uses this to pick a per-manager
/// update command; ByteAsk runs `byteask --update` for all of them (see
/// [`UpdateAction::command_args`]), so these variants now only record HOW the
/// user installed, not what to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Installed as a global npm package.
    NpmGlobalLatest,
    /// Installed as a global bun package.
    BunGlobalLatest,
    /// Update via `vp install -g @openai/codex@latest`.
    VitePlusGlobalLatest,
    /// Update via `pnpm add -g @openai/codex@latest`.
    PnpmGlobalLatest,
    /// Installed via Homebrew.
    BrewUpgrade,
    /// Installed by the standalone Unix install script.
    StandaloneUnix,
    /// Installed by the standalone Windows install script.
    StandaloneWindows,
}

impl UpdateAction {
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Npm => Some(UpdateAction::NpmGlobalLatest),
            InstallMethod::Bun => Some(UpdateAction::BunGlobalLatest),
            InstallMethod::VitePlus => Some(UpdateAction::VitePlusGlobalLatest),
            InstallMethod::Pnpm => Some(UpdateAction::PnpmGlobalLatest),
            InstallMethod::Brew => Some(UpdateAction::BrewUpgrade),
            InstallMethod::Standalone { platform, .. } => Some(match platform {
                StandalonePlatform::Unix => UpdateAction::StandaloneUnix,
                StandalonePlatform::Windows => UpdateAction::StandaloneWindows,
            }),
            InstallMethod::Other => None,
        }
    }

    /// Returns the list of command-line arguments for invoking the update.
    ///
    /// ByteAsk has ONE update path regardless of how the user installed: the
    /// wrapper's `byteask --update`, which re-runs install.sh, preserves the
    /// login token, and works identically for the curl, pip and npm installs
    /// (the pip/npm packages are thin launchers over the same runtime).
    ///
    /// The upstream per-manager commands are deliberately NOT used here. Every
    /// one of them installs a DIFFERENT product over a ByteAsk install:
    /// `npm install -g @openai/codex`, `brew upgrade --cask codex`, and the
    /// `chatgpt.com/codex/install.sh` standalone script all fetch OpenAI's
    /// Codex. Running any of them would replace the user's byteask binary.
    /// The variant is still detected and reported (`from_install_context`);
    /// only the command it maps to changes.
    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        ("byteask", &["--update"])
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command).chain(args.iter().copied()))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn maps_install_context_to_update_action() {
        let native_release_dir =
            AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join("native-release"))
                .expect("temp dir path should be absolute");

        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Other,
                package_layout: None,
            }),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }),
            Some(UpdateAction::NpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Bun,
                package_layout: None,
            }),
            Some(UpdateAction::BunGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Pnpm,
                package_layout: None,
            }),
            Some(UpdateAction::PnpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Brew,
                package_layout: None,
            }),
            Some(UpdateAction::BrewUpgrade)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Unix,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            Some(UpdateAction::StandaloneUnix)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Windows,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            Some(UpdateAction::StandaloneWindows)
        );
    }

    /// Every install method must resolve to ByteAsk's own update path.
    ///
    /// This replaces an upstream test that asserted the per-manager commands.
    /// The regression it guards is specific and severe: those commands
    /// (`npm install -g @openai/codex`, the `chatgpt.com/codex` installers)
    /// fetch OpenAI's Codex, so an upstream merge that reintroduces them would
    /// make a ByteAsk update silently replace the user's binary with a
    /// different product.
    #[test]
    fn every_install_method_updates_via_the_byteask_wrapper() {
        for action in [
            UpdateAction::NpmGlobalLatest,
            UpdateAction::BunGlobalLatest,
            UpdateAction::VitePlusGlobalLatest,
            UpdateAction::PnpmGlobalLatest,
            UpdateAction::BrewUpgrade,
            UpdateAction::StandaloneUnix,
            UpdateAction::StandaloneWindows,
        ] {
            assert_eq!(action.command_args(), ("byteask", &["--update"][..]));
            let rendered = action.command_str();
            assert_eq!(rendered, "byteask --update");
            assert!(
                !rendered.contains("codex") && !rendered.contains("chatgpt"),
                "update command leaks the upstream product: {rendered}"
            );
        }
    }
}
