//! Offline replay driver (test-only): run `cpp_impact` on a repository and print JSON.
//!
//! ```text
//! BYTEASK_IMPACT_REPLAY=/path/to/repo [BYTEASK_IMPACT_REPLAY_MODE=impact|changes] \
//!   <test-binary> impact::replay::impact_replay --ignored --exact --nocapture
//! ```
//!
//! `impact` prints the checklist of the working tree against HEAD (items, changed entities,
//! rendered text); `changes` prints only the changed entities (used to describe a reference
//! patch). No network, no model.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use super::*;

fn entity_json(change: &Changed) -> serde_json::Value {
    json!({
        "path": change.path,
        "name": change.entity.name,
        "qualified": change.entity.qualified(),
        "signature": change.entity.signature(),
        "kind": format!("{:?}", change.entity.kind),
        "line": change.entity.line,
        "end_line": change.entity.end_line,
        "added": change.added,
        "param_types": change.entity.param_types,
    })
}

#[tokio::test]
#[ignore = "replay driver, run explicitly with BYTEASK_IMPACT_REPLAY"]
async fn impact_replay() {
    let Ok(root) = std::env::var("BYTEASK_IMPACT_REPLAY") else {
        return;
    };
    let mode = std::env::var("BYTEASK_IMPACT_REPLAY_MODE").unwrap_or_else(|_| "impact".to_string());
    let root = PathBuf::from(root);
    let preexisting: Vec<String> = std::env::var("BYTEASK_IMPACT_REPLAY_UNTRACKED")
        .ok()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .map(|text| {
            text.lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let started = std::time::Instant::now();
    if mode == "changes" {
        let changes = collect_changes(&root, "HEAD", &preexisting).await;
        let mut changed = Vec::new();
        for change in &changes {
            changed.extend(change.changed_entities());
        }
        dedupe_changed(&mut changed);
        let out = json!({
            "files": changes.iter().map(|c| c.path.clone()).collect::<Vec<_>>(),
            "changed": changed.iter().map(entity_json).collect::<Vec<_>>(),
        });
        println!("BYTEASK_REPLAY_JSON {out}");
        return;
    }
    let req = ImpactRequest {
        root: root.clone(),
        base_rev: "HEAD".to_string(),
        source: Source::Diff,
        budget: Duration::from_secs(
            std::env::var("BYTEASK_IMPACT_REPLAY_BUDGET")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120),
        ),
        use_clangd: true,
        max_items: DEFAULT_MAX_ITEMS,
        preexisting_untracked: preexisting,
    };
    let report = analyze(&req).await;
    let mut numbering = Numbering::default();
    let rendered = render(&report, &mut numbering, "cpp_impact", None);
    // Every entity at each item's location, for scoring against a reference patch.
    let mut item_entities = Vec::new();
    for item in &report.items {
        let entity = std::fs::read_to_string(root.join(&item.path))
            .ok()
            .and_then(|text| {
                let scan = scan::scan(&text);
                scan.entity_at(item.line)
                    .map(|e| (e.qualified(), e.param_types.clone()))
            });
        let (entity, entity_params) = match entity {
            Some((name, params)) => (Some(name), Some(params)),
            None => (None, None),
        };
        item_entities.push(json!({
            "facet": format!("{:?}", item.facet),
            "key": item.key,
            "path": item.path,
            "line": item.line,
            "text": item.text,
            "entity": entity,
            "entity_params": entity_params,
            "confirmed": item.confirmed,
        }));
    }
    let out = json!({
        "engine": report.engine,
        "files_changed": report.files_changed,
        "changed": report.changed.iter().map(entity_json).collect::<Vec<_>>(),
        "items": item_entities,
        "notes": report.notes,
        "generated_copies": report.generated_copies,
        "rendered": rendered,
        "rendered_bytes": rendered.len(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    println!("BYTEASK_REPLAY_JSON {out}");
}
