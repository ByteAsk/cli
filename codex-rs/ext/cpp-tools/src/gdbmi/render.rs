//! Render machine stop records as the tidy, human-readable lines a developer
//! expects — never raw MI tuples.

use serde_json::Value;

/// Map an MI stop `reason` to plain English.
fn friendly_reason(reason: &str) -> &str {
    match reason {
        "breakpoint-hit" => "breakpoint hit",
        "watchpoint-trigger" => "watchpoint triggered",
        "read-watchpoint-trigger" => "read watchpoint triggered",
        "access-watchpoint-trigger" => "access watchpoint triggered",
        "watchpoint-scope" => "watchpoint went out of scope",
        "end-stepping-range" => "step complete",
        "function-finished" => "function returned",
        "location-reached" => "location reached",
        "solib-event" => "shared-library event",
        "signal-received" => "signal received",
        other => other,
    }
}

/// Render a frame as `func (file:line)`, degrading gracefully when the binary
/// has no debug info (no file/line) — then `func (0xaddr)` or just `func`.
fn render_frame(frame: &Value) -> String {
    let func = frame
        .get("func")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("??");
    match (
        frame.get("file").and_then(Value::as_str),
        frame.get("line").and_then(Value::as_str),
    ) {
        (Some(file), Some(line)) => format!("{func} ({file}:{line})"),
        _ => match frame.get("addr").and_then(Value::as_str) {
            Some(addr) => format!("{func} ({addr})"),
            None => func.to_string(),
        },
    }
}

/// Render an `*stopped` payload as a single clean summary line.
pub(crate) fn render_stop(payload: &Value) -> String {
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("stopped");

    if reason.starts_with("exited") {
        return match (reason, payload.get("exit-code").and_then(Value::as_str)) {
            ("exited-normally", _) => "Program exited normally.".to_string(),
            // Killed by a signal gdb passed on — e.g. `continue` after a
            // SIGSEGV stop. It used to read "Program exited.", which a model
            // takes for a clean exit.
            ("exited-signalled", _) => {
                let signal = payload.get("signal-name").and_then(Value::as_str).unwrap_or("a signal");
                match payload.get("signal-meaning").and_then(Value::as_str) {
                    Some(meaning) if !meaning.is_empty() => {
                        format!("Program terminated by {signal} ({meaning}).")
                    }
                    _ => format!("Program terminated by {signal}."),
                }
            }
            (_, Some(code)) => format!("Program exited with code {code}."),
            _ => "Program exited.".to_string(),
        };
    }

    let mut head = if reason == "signal-received" {
        match payload.get("signal-name").and_then(Value::as_str) {
            Some(sig) => match payload.get("signal-meaning").and_then(Value::as_str) {
                Some(meaning) if !meaning.is_empty() => format!("Stopped: {sig} ({meaning})"),
                _ => format!("Stopped: {sig}"),
            },
            None => "Stopped: signal received".to_string(),
        }
    } else {
        format!("Stopped: {}", friendly_reason(reason))
    };

    if let Some(frame) = payload.get("frame") {
        head.push_str(" — ");
        head.push_str(&render_frame(frame));
    }
    head
}

/// The coarse state a `*stopped` record leaves the session in.
pub(crate) fn state_after_stop(payload: &Value) -> &'static str {
    let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("");
    if reason.starts_with("exited") { "exited" } else { "stopped" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_program_killed_by_a_signal_did_not_exit() {
        let payload = serde_json::json!({
            "reason": "exited-signalled",
            "signal-name": "SIGSEGV",
            "signal-meaning": "Segmentation fault",
        });
        assert_eq!(render_stop(&payload), "Program terminated by SIGSEGV (Segmentation fault).");
        assert_eq!(state_after_stop(&payload), "exited");
        assert_eq!(state_after_stop(&serde_json::json!({"reason": "breakpoint-hit"})), "stopped");
    }
    use serde_json::json;

    #[test]
    fn breakpoint_stop_reads_naturally() {
        let payload = json!({
            "reason": "breakpoint-hit",
            "frame": { "func": "main", "file": "demo.c", "line": "42" }
        });
        assert_eq!(
            render_stop(&payload),
            "Stopped: breakpoint hit — main (demo.c:42)"
        );
    }

    #[test]
    fn signal_includes_meaning() {
        let payload = json!({
            "reason": "signal-received",
            "signal-name": "SIGSEGV",
            "signal-meaning": "Segmentation fault",
            "frame": { "func": "crash", "file": "demo.c", "line": "7" }
        });
        assert_eq!(
            render_stop(&payload),
            "Stopped: SIGSEGV (Segmentation fault) — crash (demo.c:7)"
        );
    }

    #[test]
    fn exit_normal_and_coded() {
        assert_eq!(
            render_stop(&json!({ "reason": "exited-normally" })),
            "Program exited normally."
        );
        assert_eq!(
            render_stop(&json!({ "reason": "exited", "exit-code": "01" })),
            "Program exited with code 01."
        );
    }

    #[test]
    fn frame_without_debug_info_uses_address() {
        let payload = json!({
            "reason": "signal-received",
            "signal-name": "SIGSEGV",
            "frame": { "func": "??", "addr": "0xdeadbeef" }
        });
        assert_eq!(render_stop(&payload), "Stopped: SIGSEGV — ?? (0xdeadbeef)");
    }
}
