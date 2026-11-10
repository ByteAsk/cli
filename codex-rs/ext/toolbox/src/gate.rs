//! The toolbox plan gate.
//!
//! Inside Claude Code or Codex the model is not ByteAsk's, and for such
//! your-own-model sessions the debugger, sanitizer and performance
//! tools need Pro. Everything else (static analysis, reference lookups) is free.
//!
//! * The gate only acts while the served switch is ON. The switch is OFF
//!   unless the gateway says otherwise, so a gate bug is switched off without a
//!   release, and today (no server serves it yet) nothing changes for anyone.
//! * The plan is the last one the gateway reported, cached for 7 days of
//!   offline use; after that the free plan applies until it can check again.
//!   The cache is not signed: the client can be copied, so this gate is
//!   for honest users and DRM would buy nothing.
//! * Never signed in: paid tools refuse with the sign-in step; free tools run.
//!
//! The decision is a pure function of [`GateState`] and the clock, so every
//! case is tested without a network.

use byteask_policy::PlanCache;

/// The tools that need Pro when the model is not ByteAsk's (`debug*`,
/// `cpp_sanitize`, `cpp_perf`, `cpp_bench`).
pub const PAID_TOOLS: &[&str] = &[
    "debug",
    "debug_session",
    "debug_attach",
    "debug_record",
    "cpp_sanitize",
    "cpp_perf",
    "cpp_bench",
];

/// How long a cached plan counts without a fresh check.
pub const PLAN_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Same ranking as the gateway's `ratelimit.PLAN_RANK`; an unknown plan ranks
/// with free, exactly as the gateway's `entitled()` does.
fn plan_rank(plan: &str) -> u8 {
    match plan {
        "pro" => 1,
        "max" => 2,
        "ultra" => 3,
        _ => 0,
    }
}

/// Who is signed in, as far as the toolbox can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// No ByteAsk token in the config.
    SignedOut,
    /// A token, identified by its account fingerprint.
    SignedIn { account: String },
    /// The gateway rejected the token (expired or revoked).
    Expired,
}

/// Everything the gate decision depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateState {
    /// The served switch: the gate does nothing while it is off.
    pub switch_on: bool,
    pub identity: Identity,
    pub cache: Option<PlanCache>,
}

impl GateState {
    /// The state before anything was fetched: off.
    pub fn off() -> Self {
        Self {
            switch_on: false,
            identity: Identity::SignedOut,
            cache: None,
        }
    }
}

/// What the gate says about one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Not signed in.
    SignIn,
    /// Signed in, but the gateway rejected the token.
    SignInExpired,
    /// Signed in on a plan below Pro. `unconfirmed` = the free plan applies
    /// only because no check succeeded in the last 7 days.
    Upgrade {
        plan: String,
        unconfirmed: bool,
    },
}

pub fn is_paid(tool: &str) -> bool {
    PAID_TOOLS.contains(&tool)
}

/// The gate decision for calling `tool` at unix time `now`.
pub fn decide(tool: &str, state: &GateState, now: u64) -> Decision {
    if !state.switch_on || !is_paid(tool) {
        return Decision::Allow;
    }
    let account = match &state.identity {
        Identity::SignedOut => return Decision::SignIn,
        Identity::Expired => return Decision::SignInExpired,
        Identity::SignedIn { account } => account,
    };
    let fresh = state.cache.as_ref().filter(|cache| {
        &cache.account == account && now.saturating_sub(cache.checked_at) <= PLAN_CACHE_TTL_SECS
    });
    match fresh {
        Some(cache) if plan_rank(&cache.plan) >= plan_rank("pro") => Decision::Allow,
        Some(cache) => Decision::Upgrade {
            plan: cache.plan.clone(),
            unconfirmed: false,
        },
        None => Decision::Upgrade {
            plan: "free".to_string(),
            unconfirmed: true,
        },
    }
}

/// Tools that need nothing, named in every refusal so the dead end is not total.
const FREE_TOOLS: &str = "cpp_check, cpp_tidy, cpp_nav, cpp_layout, cpp_format, cpp_godbolt, \
                          cpp_insights, cpp_binary, cpp_symbolize, cpp_decompile, cpp_compile_db, \
                          cpp_intrinsic and the *_ref documentation tools";

/// The shared result for a refusal: NOT RUN (needs a plan) with the one
/// step that makes the tool run.
pub fn refusal_result(tool: &str, decision: &Decision) -> check_result::CheckResult {
    let fix = match decision {
        Decision::Allow | Decision::SignIn => {
            "sign in with `byteask login` in a terminal, then call the tool again."
        }
        Decision::SignInExpired => {
            "run `byteask` once in a terminal to renew the sign-in, then call the tool again."
        }
        Decision::Upgrade {
            unconfirmed: false, ..
        } => "run `byteask` in a terminal and type /upgrade.",
        Decision::Upgrade {
            unconfirmed: true, ..
        } => "connect to the internet and run `byteask policy refresh-plan`.",
    };
    check_result::CheckResult::not_run(
        tool,
        check_result::NotRunReason::NeedsPlan,
        "This tool needs ByteAsk Pro when ByteAsk runs inside another agent; nothing was run.",
        fix,
    )
}

/// The model-facing refusal for a non-`Allow` decision.
pub fn refusal_message(tool: &str, decision: &Decision) -> String {
    let why = format!(
        "{tool} is part of ByteAsk Pro when ByteAsk's tools run inside another agent, because \
         the model here is not ByteAsk's."
    );
    let step = match decision {
        Decision::Allow => return String::new(),
        Decision::SignIn => "You are not signed in to ByteAsk. Sign in by running `byteask login` \
                             in a terminal, then call the tool again."
            .to_string(),
        Decision::SignInExpired => "Your ByteAsk sign-in has expired. Run `byteask` once in a \
                                    terminal to renew it, then call the tool again."
            .to_string(),
        Decision::Upgrade {
            plan,
            unconfirmed: false,
        } => format!(
            "This account is on the {plan} plan. To upgrade, run `byteask` in a terminal and type \
             /upgrade."
        ),
        Decision::Upgrade {
            unconfirmed: true, ..
        } => "ByteAsk could not confirm your plan in the last 7 days, so the free plan applies \
              until it can check again. Connect to the internet and run `byteask policy \
              refresh-plan`, or restart your agent."
            .to_string(),
    };
    format!(
        "{why} {step}\n\nNothing was run. These tools need no plan and work now: {FREE_TOOLS}. \
         Tell the user which step to take."
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn cache(plan: &str, age_secs: u64) -> PlanCache {
        PlanCache {
            plan: plan.to_string(),
            checked_at: NOW - age_secs,
            account: "acct".to_string(),
            gate: true,
        }
    }

    fn state(switch_on: bool, identity: Identity, cache: Option<PlanCache>) -> GateState {
        GateState {
            switch_on,
            identity,
            cache,
        }
    }

    fn signed_in() -> Identity {
        Identity::SignedIn {
            account: "acct".to_string(),
        }
    }

    #[test]
    fn switch_off_changes_nothing_for_anyone() {
        for identity in [Identity::SignedOut, Identity::Expired, signed_in()] {
            for tool in PAID_TOOLS {
                assert_eq!(
                    decide(tool, &state(false, identity.clone(), None), NOW),
                    Decision::Allow
                );
            }
        }
    }

    #[test]
    fn free_tools_never_need_a_plan() {
        let off_the_grid = state(true, Identity::SignedOut, None);
        for tool in [
            "cpp_check",
            "cpp_tidy",
            "hft_ref",
            "ref_context",
            "cpp_godbolt",
            "cpp_kdb",
        ] {
            assert_eq!(decide(tool, &off_the_grid, NOW), Decision::Allow, "{tool}");
        }
    }

    #[test]
    fn never_signed_in_refuses_paid_tools_with_the_sign_in_step() {
        let gate = state(true, Identity::SignedOut, None);
        assert_eq!(decide("debug_session", &gate, NOW), Decision::SignIn);
        let text = refusal_message("debug_session", &Decision::SignIn);
        assert!(text.contains("byteask login"), "{text}");
        assert!(text.contains("cpp_check"), "{text}");
    }

    #[test]
    fn free_plan_is_refused_and_pro_and_above_are_allowed() {
        let free = state(true, signed_in(), Some(cache("free", 60)));
        assert_eq!(
            decide("debug", &free, NOW),
            Decision::Upgrade {
                plan: "free".to_string(),
                unconfirmed: false
            }
        );
        for plan in ["pro", "max", "ultra"] {
            let paid = state(true, signed_in(), Some(cache(plan, 60)));
            assert_eq!(
                decide("cpp_sanitize", &paid, NOW),
                Decision::Allow,
                "{plan}"
            );
        }
    }

    #[test]
    fn an_unknown_plan_name_ranks_with_free_like_the_gateway() {
        let odd = state(true, signed_in(), Some(cache("enterprise-beta", 60)));
        assert!(matches!(
            decide("cpp_perf", &odd, NOW),
            Decision::Upgrade { .. }
        ));
    }

    #[test]
    fn a_cached_pro_plan_works_offline_for_seven_days_then_free_applies() {
        let day = 24 * 60 * 60;
        let six_days = state(true, signed_in(), Some(cache("pro", 6 * day)));
        assert_eq!(decide("debug", &six_days, NOW), Decision::Allow);
        let seven_days = state(true, signed_in(), Some(cache("pro", 7 * day)));
        assert_eq!(decide("debug", &seven_days, NOW), Decision::Allow);
        let eight_days = state(true, signed_in(), Some(cache("pro", 8 * day)));
        assert_eq!(
            decide("debug", &eight_days, NOW),
            Decision::Upgrade {
                plan: "free".to_string(),
                unconfirmed: true
            }
        );
        let text = refusal_message("debug", &decide("debug", &eight_days, NOW));
        assert!(text.contains("7 days"), "{text}");
        assert!(text.contains("byteask policy refresh-plan"), "{text}");
    }

    #[test]
    fn another_accounts_cached_plan_does_not_carry_over() {
        let mut other = cache("ultra", 60);
        other.account = "someone-else".to_string();
        let gate = state(true, signed_in(), Some(other));
        assert!(matches!(
            decide("debug_attach", &gate, NOW),
            Decision::Upgrade {
                unconfirmed: true,
                ..
            }
        ));
    }

    #[test]
    fn an_expired_sign_in_names_the_renewal_step() {
        let gate = state(true, Identity::Expired, Some(cache("pro", 60)));
        assert_eq!(decide("cpp_bench", &gate, NOW), Decision::SignInExpired);
        assert!(refusal_message("cpp_bench", &Decision::SignInExpired).contains("expired"));
    }

    #[test]
    fn exactly_seven_tools_are_paid() {
        // That each one is a real registry tool is checked against the shared
        // registry in `server::tests`, where a typo would leave a tool ungated.
        assert_eq!(PAID_TOOLS.len(), 7);
    }
}
