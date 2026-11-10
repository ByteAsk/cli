//! Tool descriptions, parameter docs and guidance are PROTECTED. This
//! pins every model-facing byte of the context tools and their guidance.
//! When it fails, the diff IS the review: get a maintainer's sign-off, and only
//! then regenerate with
//! `BYTEASK_UPDATE_TOOL_SNAPSHOT=1 cargo test -p codex-context-tools description_snapshot`.
//!
//! A second test keeps the text identical to the context engine's own pinned
//! list (`snapshots/bctx_tools.json`, a copy of its
//! `tests/snapshots/tools.json`), so the two cannot drift apart.

use pretty_assertions::assert_eq;

const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/snapshots/model_facing_text.json"
);

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

fn current() -> String {
    let tools: Vec<serde_json::Value> = crate::tools::specs()
        .iter()
        .map(|spec| sorted_keys(serde_json::to_value(spec).expect("spec serialises")))
        .collect();
    let doc = serde_json::json!({
        "guidance": crate::guidance::CONTEXT_GUIDANCE,
        "tools": tools,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&sorted_keys(doc)).expect("json")
    )
}

#[test]
fn description_snapshot_is_byte_identical() {
    let now = current();
    if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
        std::fs::write(SNAPSHOT, &now).expect("write snapshot");
        return;
    }
    let pinned = std::fs::read_to_string(SNAPSHOT)
        .expect("snapshot missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1");
    assert_eq!(
        pinned, now,
        "model-facing tool text changed; see the module comment"
    );
}

/// Names, order, tool descriptions, parameter names and types, required
/// parameters and every parameter description the context engine writes are
/// the context engine's own. Integer parameters, whose range our schema type
/// cannot carry, must state that range in their description.
#[test]
fn the_text_is_the_context_engines_own() {
    let theirs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("snapshots/bctx_tools.json")).expect("json");
    let ours: Vec<serde_json::Value> = crate::tools::specs()
        .iter()
        .map(|spec| serde_json::to_value(spec).expect("spec"))
        .collect();
    assert_eq!(theirs.len(), ours.len());
    for (theirs, ours) in theirs.iter().zip(&ours) {
        let name = theirs["name"].as_str().expect("name");
        assert_eq!(ours["name"], theirs["name"]);
        assert_eq!(ours["description"], theirs["description"], "{name}");
        let schema = &theirs["inputSchema"];
        assert_eq!(ours["parameters"]["required"], schema["required"], "{name}");
        let their_props = schema["properties"].as_object().expect("properties");
        let our_props = ours["parameters"]["properties"]
            .as_object()
            .expect("properties");
        let mut their_names: Vec<&String> = their_props.keys().collect();
        let mut our_names: Vec<&String> = our_props.keys().collect();
        their_names.sort();
        our_names.sort();
        assert_eq!(our_names, their_names, "{name}");
        for (prop, their_schema) in their_props {
            let our_schema = &our_props[prop];
            assert_eq!(our_schema["type"], their_schema["type"], "{name}.{prop}");
            assert_eq!(
                our_schema.get("enum"),
                their_schema.get("enum"),
                "{name}.{prop}"
            );
            if let Some(description) = their_schema.get("description") {
                assert_eq!(&our_schema["description"], description, "{name}.{prop}");
            }
            if let (Some(min), Some(max)) =
                (their_schema.get("minimum"), their_schema.get("maximum"))
            {
                let text = our_schema["description"].as_str().unwrap_or_default();
                assert!(
                    text.contains(&format!("{min}-{max}")),
                    "{name}.{prop}: the description must state the range {min}-{max}: {text:?}"
                );
            }
        }
    }
}
