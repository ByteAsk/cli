//! ByteAsk rotating footer hints — the always-on one-liner under the composer
//! that cycles a handful of tips (usage, voucher, referral, own-key, gdb bridge,
//! upgrade) every [`ROTATE_SECS`]. Pure content + rotation math here;
//! the composer renders it and drives the timer. Only shown for ByteAsk gateway
//! accounts.

/// Seconds each hint stays before rotating to the next.
pub(crate) const ROTATE_SECS: u64 = 12;

/// The rotating hint set. The usage slot shows the live weekly-quota percentage
/// when known, else a short `/usage` nudge; the rest are steady one-liners for the
/// key slash commands and the gdb bridge. Kept factual, not markety.
fn hints(usage_week_pct: Option<i64>) -> Vec<String> {
    let usage = match usage_week_pct {
        Some(pct) => format!("{pct}% of your weekly quota used — type /usage for the full picture"),
        None => "Type /usage to view your quota".to_string(),
    };
    vec![
        usage,
        "Redeem a voucher code with /voucher <code>".to_string(),
        "Refer a friend: you both get $10 of usage for 30 days (/referral-code)".to_string(),
        "Set your own API key or subscription with /login".to_string(),
        "Attach ByteAsk to your live gdb session — run byteask-bridge in gdb".to_string(),
        "Need higher limits? Upgrade your plan with /upgrade".to_string(),
    ]
}

/// The hint to show at the given rotation `tick` (= elapsed_secs / ROTATE_SECS).
/// Cycles through [`hints`]; `usage_week_pct` fills the live usage slot.
pub(crate) fn hint_for(tick: u64, usage_week_pct: Option<i64>) -> String {
    let hs = hints(usage_week_pct);
    let idx = (tick % hs.len() as u64) as usize;
    hs[idx].clone()
}

/// How many distinct hints are in the rotation (for tests / callers).
#[cfg(test)]
pub(crate) fn hint_count() -> usize {
    hints(None).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_wrap_and_consecutive_differ() {
        let n = hint_count();
        assert_eq!(n, 6);
        assert_eq!(hint_for(0, None), hint_for(n as u64, None)); // wrap-around
        assert_ne!(hint_for(0, None), hint_for(1, None)); // consecutive differ
        for t in 0..n as u64 {
            assert!(!hint_for(t, None).trim().is_empty());
        }
    }

    #[test]
    fn usage_slot_uses_live_pct_when_known_else_nudge() {
        assert!(hint_for(0, Some(42)).contains("42%"));
        assert!(hint_for(0, Some(42)).contains("/usage"));
        assert!(hint_for(0, None).contains("/usage"));
        assert!(!hint_for(0, None).contains('%'));
    }

    #[test]
    fn each_command_hint_present_exactly_once() {
        let all: Vec<String> = (0..hint_count() as u64)
            .map(|t| hint_for(t, None))
            .collect();
        for needle in [
            "/voucher",
            "/referral-code",
            "/login",
            "byteask-bridge",
            "/upgrade",
        ] {
            assert_eq!(
                all.iter().filter(|h| h.contains(needle)).count(),
                1,
                "expected exactly one hint containing {needle:?}"
            );
        }
    }
}
