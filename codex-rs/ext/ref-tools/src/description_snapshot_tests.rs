//! Tool descriptions, parameter docs and guidance are
//! PROTECTED. They are what makes the model pick the right tool with the right arguments,
//! so no refactor, upstream merge or "token saving" may change one silently.
//!
//! This test pins every model-facing byte. When it fails, the diff IS the review: check
//! that no routing cue, constraint, default, unit, example or caveat was lost, have the
//! change reviewed, and only then regenerate with
//! `BYTEASK_UPDATE_TOOL_SNAPSHOT=1 cargo test -p codex-ref-tools description_snapshot`.

use pretty_assertions::assert_eq;

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/snapshots/model_facing_text.json");

fn current() -> String {
    let mut tools: Vec<serde_json::Value> = crate::tools::ref_tools()
        .iter()
        .map(|tool| sorted_keys(serde_json::to_value(tool.spec()).expect("spec serialises")))
        .collect();
    tools.sort_by_key(|t| t.to_string());
    let doc = serde_json::json!({
        "guidance": crate::guidance::REF_GUIDANCE,
        "tools": tools,
    });
    format!("{}\n", serde_json::to_string_pretty(&doc).expect("json"))
}

/// Keys sorted at every level. The pinned file was written with sorted keys;
/// a build that also compiles a crate turning on serde_json's `preserve_order`
/// (the TUI, the toolbox) serialises in insertion order instead, which changed
/// the rendering, and the tool order, without any text changing.
fn sorted_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sorted_keys(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted_keys).collect())
        }
        other => other,
    }
}

#[test]
fn description_snapshot_is_byte_identical() {
    let now = current();
    if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
        std::fs::create_dir_all(std::path::Path::new(SNAPSHOT).parent().expect("parent"))
            .expect("mkdir");
        std::fs::write(SNAPSHOT, &now).expect("write snapshot");
        return;
    }
    let pinned = std::fs::read_to_string(SNAPSHOT)
        .expect("snapshot missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1");
    assert_eq!(pinned, now, "model-facing tool text changed; see the module comment");
}

#[test]
fn description_snapshot_covers_every_reference_tool() {
    assert_eq!(crate::tools::ref_tools().len(), 9);
}
