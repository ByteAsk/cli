//! ByteAsk loop NUDGE. Never a block.
//!
//! When the same (tool, arguments) has just returned the SAME output several times in a
//! row, the next result carries one line saying so. The call still runs: identical
//! commands are often legitimate (`git status`, re-running tests after an edit, polling a
//! deploy), which is why this keys on CONSECUTIVE identical calls with identical OUTPUT,
//! is reset by any other call, and ignores `write_stdin` polls entirely.
//!
//! Off unless the server-driven policy turns `loop_nudge` on.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;

/// The nudge appears on this repeat and every one after it.
const NUDGE_FROM: u32 = 4;
const MAX_THREADS: usize = 256;

#[derive(Default)]
struct Last {
    key: u64,
    output: u64,
    count: u32,
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    h.finish()
}

/// The part of an output that says something about the WORLD. Every exec result starts
/// with a random `Chunk ID` and a wall time, so two runs of the same failing command are
/// never byte-identical; found by the end-to-end test, where the nudge never fired.
pub(crate) fn stable_part(output: &str) -> String {
    output
        .lines()
        .filter(|l| {
            !(l.starts_with("Chunk ID:")
                || l.starts_with("Wall time:")
                || l.starts_with("[exit ")
                || l.starts_with("[running "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Record one finished call. Returns how many times IN A ROW this exact call has now
/// returned this exact output (1 for the first time).
fn record(state: &mut HashMap<String, Last>, thread: &str, tool: &str, args: &str, output: &str) -> u32 {
    if state.len() >= MAX_THREADS && !state.contains_key(thread) {
        state.clear();
    }
    let key = hash_of(&[tool, args]);
    let out = hash_of(&[output]);
    let last = state.entry(thread.to_string()).or_default();
    if last.key == key && last.output == out {
        last.count += 1;
    } else {
        *last = Last { key, output: out, count: 1 };
    }
    last.count
}

pub(crate) fn nudge_line(count: u32) -> String {
    format!(
        "\n[byteask: this is identical call number {count} in a row and its output has not changed, so repeating it will not give new information. Try a different approach, or tell the user what is blocking you.]"
    )
}

fn output_text(item: &ResponseItem) -> Option<String> {
    let ResponseItem::FunctionCallOutput { output, .. } = item else {
        return None;
    };
    Some(match &output.body {
        FunctionCallOutputBody::Text(t) => t.clone(),
        FunctionCallOutputBody::ContentItems(items) => items
            .iter()
            .filter_map(|i| match i {
                FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

/// Observe a finished tool call and, when warranted, append the nudge to its output.
pub(crate) fn observe(thread: &str, tool: &str, args: &str, item: &mut ResponseItem) {
    if tool == "write_stdin" || !byteask_output_reducers::policy::active().flag("loop_nudge") {
        return;
    }
    let Some(text) = output_text(item) else { return };
    static STATE: OnceLock<Mutex<HashMap<String, Last>>> = OnceLock::new();
    let count = {
        let Ok(mut state) = STATE.get_or_init(Default::default).lock() else { return };
        record(&mut state, thread, tool, args, &stable_part(&text))
    };
    if count >= NUDGE_FROM {
        append(item, &nudge_line(count));
    }
}

/// MCP and connector tools (the only NAMESPACED tools) often relay terminal output. They
/// get the two terminal-equivalent transforms and nothing else; the original is spilled
/// under the call id. Built-in tools, file reads, patches, images and ByteAsk's own
/// extension tools have no namespace and are never touched here.
pub(crate) fn reduce_namespaced_output(namespace: Option<&str>, call_id: &str, item: &mut ResponseItem) {
    let Some(namespace) = namespace else { return };
    if namespace == "collaboration" {
        return;
    }
    let policy = byteask_output_reducers::policy::active();
    let ResponseItem::FunctionCallOutput { output, .. } = item else { return };
    // MCP results arrive as a LIST of text parts, not one string (the shape that made an
    // earlier project compress nothing on real traffic). Handle both.
    match &mut output.body {
        FunctionCallOutputBody::Text(text) => {
            let reduced = byteask_output_reducers::reduce_terminal(text, policy.ansi, policy.cr);
            if !reduced.changed() {
                return;
            }
            let footer = namespaced_footer(&reduced.fired.join(" "), call_id, text.as_bytes());
            *text = format!("{}\n{footer}", reduced.text);
        }
        FunctionCallOutputBody::ContentItems(items) => {
            let mut original = String::new();
            let mut fired: Vec<&'static str> = Vec::new();
            let mut replaced: Vec<(usize, String)> = Vec::new();
            for (index, part) in items.iter().enumerate() {
                let FunctionCallOutputContentItem::InputText { text } = part else { continue };
                original.push_str(text);
                original.push('\n');
                let reduced = byteask_output_reducers::reduce_terminal(text, policy.ansi, policy.cr);
                if reduced.changed() {
                    for name in reduced.fired {
                        if !fired.contains(&name) {
                            fired.push(name);
                        }
                    }
                    replaced.push((index, reduced.text));
                }
            }
            if replaced.is_empty() {
                return;
            }
            let footer = namespaced_footer(&fired.join(" "), call_id, original.as_bytes());
            for (index, text) in replaced {
                items[index] = FunctionCallOutputContentItem::InputText { text };
            }
            items.push(FunctionCallOutputContentItem::InputText { text: footer });
        }
    }
}

fn namespaced_footer(notes: &str, call_id: &str, original: &[u8]) -> String {
    match byteask_output_reducers::spill::write(call_id, original) {
        Some(path) => format!("[byteask: {notes}; original saved at {}]", path.display()),
        None => format!("[byteask: {notes}; original not retained]"),
    }
}

fn append(item: &mut ResponseItem, line: &str) {
    let ResponseItem::FunctionCallOutput { output, .. } = item else { return };
    match &mut output.body {
        FunctionCallOutputBody::Text(t) => t.push_str(line),
        FunctionCallOutputBody::ContentItems(items) => {
            items.push(FunctionCallOutputContentItem::InputText { text: line.trim_start().to_string() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn only_consecutive_identical_calls_with_identical_output_count() {
        let mut s = HashMap::new();
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "err"), 1);
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "err"), 2);
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "err"), 3);
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "err"), 4);
        // The output changed: progress was made, start again.
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "different"), 1);
        // Any other call in between resets it (it may have changed state).
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "different"), 2);
        assert_eq!(record(&mut s, "t", "apply_patch", "p", "ok"), 1);
        assert_eq!(record(&mut s, "t", "exec", "{\"cmd\":\"make\"}", "different"), 1);
        // Threads are independent.
        assert_eq!(record(&mut s, "other", "exec", "{\"cmd\":\"make\"}", "different"), 1);
    }

    fn output_item(text: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("c".to_string()),
            name: None,
            namespace: None,
            output: codex_protocol::models::FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(text.to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn only_namespaced_tools_are_reduced_and_only_terminal_equivalently() {
        let coloured = format!("\u{1b}[32mok\u{1b}[0m done\n{}", "row of plain text\n".repeat(60));
        let mut builtin = output_item(&coloured);
        reduce_namespaced_output(None, "call_a", &mut builtin);
        assert_eq!(output_text(&builtin).unwrap(), coloured, "no namespace: untouched");

        let mut mcp = output_item(&coloured);
        reduce_namespaced_output(Some("github"), "call_b", &mut mcp);
        let text = output_text(&mcp).unwrap();
        assert!(text.starts_with("ok done\n"));
        assert!(byteask_output_reducers::is_reduced(&text));

        // The shape MCP really uses: a list of text parts.
        let mut listed = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("c".to_string()),
            name: None,
            namespace: None,
            output: codex_protocol::models::FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText { text: "Wall time: 0.01 seconds\nOutput:".to_string() },
                    FunctionCallOutputContentItem::InputText { text: coloured.clone() },
                ]),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        };
        reduce_namespaced_output(Some("colour"), "call_d", &mut listed);
        let text = output_text(&listed).unwrap();
        assert!(!text.contains('\u{1b}'), "escape codes survived in a list-shaped output");
        assert!(text.contains("ok done"));
        assert!(byteask_output_reducers::is_reduced(&text));

        let plain = "plain\n".repeat(200);
        let mut untouched = output_item(&plain);
        reduce_namespaced_output(Some("github"), "call_c", &mut untouched);
        assert_eq!(output_text(&untouched).unwrap(), plain);
    }

    #[test]
    fn the_volatile_exec_header_does_not_hide_a_repeat() {
        let a = "Chunk ID: 11cf70\nWall time: 0.0012 seconds\nProcess exited with code 1\nOutput:\ncat: x: No such file";
        let b = "Chunk ID: 9a02be\nWall time: 0.0034 seconds\nProcess exited with code 1\nOutput:\ncat: x: No such file";
        assert_eq!(stable_part(a), stable_part(b));
        assert_eq!(stable_part("[exit 1 | 0.0s]\nboom"), stable_part("[exit 1 | 0.3s]\nboom"));
        // A different exit code or different text is a different result.
        let c = a.replace("code 1", "code 0");
        assert_ne!(stable_part(a), stable_part(&c));
        assert_ne!(stable_part(a), stable_part(&a.replace("No such file", "Permission denied")));
    }

    #[test]
    fn the_nudge_is_a_sentinel_line_so_later_passes_leave_it_alone() {
        let text = format!("output{}", nudge_line(4));
        assert!(byteask_output_reducers::is_reduced(&text));
        assert!(text.contains("identical call number 4"));
    }
}
