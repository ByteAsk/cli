use pretty_assertions::assert_eq;

use super::*;

/// A real trace: clang compiles a template-heavy file with `-ftime-trace` (granularity 0, as
/// the context engine's indexer runs it) and a light one; the card names the heavy file first and its
/// instantiations.
#[test]
fn a_real_clang_trace_names_the_slowest_file_and_its_templates() {
    if !crate::tools::binary_on_path("clang++") {
        eprintln!("skipped: clang++ not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("build/CMakeFiles/feed.dir/src");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(
        dir.path().join("heavy.cpp"),
        "#include <map>\n#include <string>\n#include <vector>\n\
         template <int N> struct Fib { static constexpr long v = Fib<N - 1>::v + Fib<N - 2>::v; };\n\
         template <> struct Fib<0> { static constexpr long v = 0; };\n\
         template <> struct Fib<1> { static constexpr long v = 1; };\n\
         long book() { std::map<std::string, std::vector<int>> m; m[\"a\"].push_back(Fib<60>::v); return m.size(); }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("light.cpp"), "int light() { return 1; }\n").unwrap();
    for name in ["heavy", "light"] {
        let status = std::process::Command::new("clang++")
            .args([
                "-std=c++17",
                "-ftime-trace",
                "-ftime-trace-granularity=0",
                "-c",
            ])
            .arg(dir.path().join(format!("{name}.cpp")))
            .arg("-o")
            .arg(tree.join(format!("{name}.cpp.o")))
            .status()
            .unwrap();
        assert!(status.success());
    }
    let costs = collect(&[dir.path().join("build")], |_| None);
    assert_eq!(costs.len(), 2, "{costs:?}");
    let card = card(&costs).expect("a card");
    let lines: Vec<&str> = card.lines().collect();
    assert!(
        lines[0].starts_with("Why the build is slow (clang -ftime-trace, 2 file(s)"),
        "{card}"
    );
    assert!(
        lines[2].ends_with("CMakeFiles/feed.dir/src/heavy.cpp"),
        "heavy first: {card}"
    );
    assert!(card.contains("Heaviest template instantiations"), "{card}");
    assert!(
        card.contains("Fib<") || card.contains("std::map<"),
        "{card}"
    );
}

/// Per-file evidence exported from the context engine's index.
#[test]
fn the_context_engines_evidence_gives_the_same_card() {
    let export = serde_json::json!([
        {"tu_path": "src/book.cpp", "evidence_json": serde_json::json!({
            "production_check": "ok", "time_trace": "collected", "frontend_us": 4_200_000,
            "heaviest_instantiations": [["class std::map<int, Order>", 900_000], ["function Book::add<Order>", 300_000]]
        }).to_string()},
        {"tu_path": "src/feed.cpp", "evidence_json": serde_json::json!({
            "frontend_us": 9_000_000,
            "heaviest_instantiations": [["class std::map<int, Order>", 1_100_000]]
        }).to_string()},
        {"tu_path": "src/gcc_only.cpp", "evidence_json": "{\"time_trace\": \"production compiler is not clang: no -ftime-trace\"}"}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tu_versions.json");
    std::fs::write(&file, export.to_string()).unwrap();
    let env = |key: &str| (key == ENV_TIME_TRACE).then(|| file.to_string_lossy().into_owned());
    let costs = collect(&[], env);
    assert_eq!(
        costs.len(),
        2,
        "a file without clang evidence has no cost: {costs:?}"
    );
    let card = card(&costs).unwrap();
    assert_eq!(
        card.lines().take(5).collect::<Vec<_>>(),
        vec![
            "Why the build is slow (clang -ftime-trace, 2 file(s), 13.2 s of front-end time):",
            "Slowest files to compile:",
            "    9.0 s  src/feed.cpp",
            "    4.2 s  src/book.cpp",
            "Heaviest template instantiations (summed over files):",
        ]
    );
    assert!(
        card.contains("    2.0 s  class std::map<int, Order> (in 2 file(s))"),
        "{card}"
    );
}

#[test]
fn no_data_means_no_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compile_commands.json"), "[]").unwrap();
    assert_eq!(collect(&[dir.path().to_path_buf()], |_| None), Vec::new());
    assert_eq!(card(&[]), None);
}
