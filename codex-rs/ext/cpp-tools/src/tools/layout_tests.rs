//! Fixture tests for `cpp_layout`. The clang dump fixture is verbatim from a
//! real `clang -Xclang -fdump-record-layouts` run; the pahole fixture mirrors
//! pahole's exact output. Live compile-and-analyze is covered by the `#[ignore]`
//! test.

use pretty_assertions::assert_eq;

use super::*;

// ---------------- clang record-layout dump ----------------

const CLANG_DUMP: &str = "\
*** Dumping AST Record Layout
         0 | struct Bad
         0 |   char a
         8 |   double b
        16 |   char c
        20 |   int d
        24 |   char e
           | [sizeof=32, dsize=32, align=8,
           |  nvsize=32, nvalign=8]

*** Dumping AST Record Layout
         0 | struct Straddle
         0 |   char[60] pad
        64 |   long v
           | [sizeof=72, dsize=72, align=8,
           |  nvsize=72, nvalign=8]
";

/// x86-64 Linux, as `clang -dM -E` reports it.
fn lp64() -> DataModel {
    DataModel::from_macros(
        "#define __SIZEOF_POINTER__ 8\n#define __SIZEOF_SHORT__ 2\n#define __SIZEOF_INT__ 4\n\
         #define __SIZEOF_LONG__ 8\n#define __SIZEOF_LONG_LONG__ 8\n#define __SIZEOF_FLOAT__ 4\n\
         #define __SIZEOF_DOUBLE__ 8\n#define __SIZEOF_LONG_DOUBLE__ 16\n#define __SIZEOF_WCHAR_T__ 4\n\
         #define __SIZEOF_INT128__ 16\n#define __SIZE_TYPE__ long unsigned int\n",
    )
}

#[test]
fn parses_two_records_with_offsets_and_size() {
    let recs = parse_clang_layouts(CLANG_DUMP, &lp64());
    assert_eq!(recs.len(), 2);
    let bad = &recs[0];
    assert_eq!(bad.name, "Bad");
    assert_eq!(bad.size, 32);
    assert_eq!(bad.align, Some(8));
    assert_eq!(bad.members.len(), 5);
    assert_eq!(
        bad.members[0],
        Member {
            name: "a".to_string(),
            offset: 0,
            size: Some(1)
        }
    );
    assert_eq!(
        bad.members[1],
        Member {
            name: "b".to_string(),
            offset: 8,
            size: Some(8)
        }
    );
    // The array member "char[60] pad" is provably sized.
    let straddle = &recs[1];
    assert_eq!(
        straddle.members[0],
        Member {
            name: "pad".to_string(),
            offset: 0,
            size: Some(60)
        }
    );
    assert_eq!(
        straddle.members[1],
        Member {
            name: "v".to_string(),
            offset: 64,
            size: Some(8)
        }
    );
}

#[test]
fn render_bad_reports_holes_and_padding_percent() {
    let recs = parse_clang_layouts(CLANG_DUMP, &lp64());
    let out = render_record(&recs[0], 64);
    assert!(out.contains("Bad: size 32, align 8  (source: clang record layout)"));
    assert!(out.contains("-- 7-byte hole --"));
    assert!(out.contains("-- 3-byte hole --"));
    assert!(out.contains("-- 7 bytes tail padding --"));
    // 15 member bytes of 32 → 17 padding, 53%.
    assert!(out.contains("17 of 32 bytes are padding (53%)."), "{out}");
    assert!(out.contains("No member straddles a 64B cacheline."));
}

// ---------------- provable-only hole math ----------------

#[test]
fn scalar_sizes_come_from_the_targets_data_model() {
    let none = HashMap::new();
    let size = |ty: &str, model: &DataModel| type_size(ty, model, &none);
    let lp64 = lp64();
    assert_eq!(size("char", &lp64), Some(1));
    assert_eq!(size("int", &lp64), Some(4));
    assert_eq!(size("unsigned long", &lp64), Some(8));
    assert_eq!(size("long double", &lp64), Some(16));
    // -m32 and a Windows target, as clang reports them: a fixed LP64 table
    // gave both x86-64 Linux sizes.
    let ilp32 = DataModel::from_macros(
        "#define __SIZEOF_POINTER__ 4\n#define __SIZEOF_LONG__ 4\n#define __SIZEOF_LONG_DOUBLE__ 12\n#define __SIZEOF_WCHAR_T__ 4\n",
    );
    assert_eq!(size("long", &ilp32), Some(4));
    assert_eq!(size("char *", &ilp32), Some(4));
    assert_eq!(size("long double", &ilp32), Some(12));
    let llp64 = DataModel::from_macros(
        "#define __SIZEOF_POINTER__ 8\n#define __SIZEOF_LONG__ 4\n#define __SIZEOF_WCHAR_T__ 2\n",
    );
    assert_eq!(size("long", &llp64), Some(4));
    assert_eq!(size("wchar_t", &llp64), Some(2));
    // No model at all: nothing is provable, never an LP64 guess.
    assert_eq!(size("long", &DataModel::default()), None);
    assert_eq!(size("struct MyStruct", &lp64), None);
}

#[test]
fn type_size_handles_pointers_arrays_records_and_unknowns() {
    let records: HashMap<&str, (u64, u64)> = [("Inner", (16, 16))].into_iter().collect();
    let size = |ty: &str| type_size(ty, &lp64(), &records);
    assert_eq!(size("unsigned int"), Some(4));
    assert_eq!(size("char[60]"), Some(60));
    assert_eq!(size("int[2][3]"), Some(24));
    assert_eq!(size("int *"), Some(8));
    assert_eq!(size("const char *"), Some(8));
    assert_eq!(size("char *[4]"), Some(32));
    assert_eq!(size("void (*)(int)"), Some(8));
    assert_eq!(size("struct Inner"), Some(16));
    assert_eq!(size("struct Inner[2]"), Some(32));
    // Not provable → None (never guess a hole).
    assert_eq!(size("struct Widget"), None);
    assert_eq!(size("enum Color"), None);
    assert_eq!(size("int Foo::*"), None);
}

#[test]
fn unknown_member_size_makes_analysis_partial() {
    // A nested unknown-size member: offsets are exact but the hole after it is
    // NOT provable → the record reports "partial", never a wrong hole.
    let rec = Record {
        name: "Mixed".to_string(),
        size: 24,
        align: Some(8),
        members: vec![
            Member {
                name: "a".to_string(),
                offset: 0,
                size: Some(4),
            },
            Member {
                name: "w".to_string(),
                offset: 8,
                size: None,
            }, // unknown (nested type)
            Member {
                name: "z".to_string(),
                offset: 16,
                size: Some(8),
            },
        ],
        source: "clang record layout",
    };
    let out = render_record(&rec, 64);
    // An unknown-size member must NEVER be rendered as a confirmed "hole"
    // (that mislabels ground truth — e.g. a struct with std::string members that
    // actually has no padding). The gap is reported as not-computed instead.
    assert!(!out.contains("hole after 'w'"), "{out}");
    assert!(out.contains("gap after 'w' not computed"), "{out}");
    assert!(out.contains("NOT necessarily padding"), "{out}");
    assert!(out.contains("hole analysis: partial"), "{out}");
    assert!(!out.contains("bytes are padding"));
}

#[test]
fn a_bitfield_without_an_extent_is_unknown_size() {
    // Older clang printed `0:0` with no bit range.
    let members = clang_members(
        &[("0:0".to_string(), "int x".to_string())],
        &lp64(),
        &HashMap::new(),
    );
    assert_eq!(
        members,
        vec![Member {
            name: "x".to_string(),
            offset: 0,
            size: None
        }]
    );
}

/// Verbatim clang 14 `-fdump-record-layouts-canonical` output.
const CLANG_CANONICAL: &str = "\
*** Dumping AST Record Layout
         0 | struct Inner
         0 |   char c
         8 |   double d
           | [sizeof=16, dsize=16, align=8,
           |  nvsize=16, nvalign=8]

*** Dumping AST Record Layout
         0 | struct Base
         0 |   (Base vtable pointer)
         8 |   int b
           | [sizeof=16, dsize=12, align=8,
           |  nvsize=12, nvalign=8]

*** Dumping AST Record Layout
         0 | struct Derived
         0 |   struct Base (primary base)
         0 |     (Base vtable pointer)
         8 |     int b
        12 |   char d
        16 |   long e
           | [sizeof=24, dsize=24, align=8,
           |  nvsize=24, nvalign=8]

*** Dumping AST Record Layout
         0 | union Mix::(unnamed at k.cpp:5:107)
         0 |   int i
         0 |   float f
           | [sizeof=4, dsize=4, align=4,
           |  nvsize=4, nvalign=4]

*** Dumping AST Record Layout
         0 | struct Mix
         0 |   char c
         8 |   struct Inner in
         8 |     char c
        16 |     double d
        24 |   struct Inner[2] arr
        56 |   long l
        64 |   wchar_t w
        72 |   void * p
    80:0-2 |   int bits
    80:3-9 |   unsigned int more
        84 |   union Mix::(unnamed at k.cpp:5:107) u
        84 |     int i
        84 |     float f
        96 |   long double ld
           | [sizeof=112, dsize=112, align=16,
           |  nvsize=112, nvalign=16]
";

#[test]
fn clang_members_are_the_direct_ones_with_exact_sizes() {
    let recs = parse_clang_layouts(CLANG_CANONICAL, &lp64());
    let members = |name: &str| -> Vec<(String, u64, Option<u64>)> {
        recs.iter()
            .find(|r| r.name == name)
            .unwrap()
            .members
            .iter()
            .map(|m| (m.name.clone(), m.offset, m.size))
            .collect()
    };
    let own = |name: &str, offset: u64, size: u64| (name.to_string(), offset, Some(size));
    // The vptr is a member; the base occupies its dsize; its own `b` is not
    // Derived's member.
    assert_eq!(members("Base"), vec![own("(vptr)", 0, 8), own("b", 8, 4)]);
    assert_eq!(
        members("Derived"),
        vec![own("(base Base)", 0, 12), own("d", 12, 1), own("e", 16, 8)]
    );
    assert_eq!(
        members("Mix"),
        vec![
            own("c", 0, 1),
            own("in", 8, 16),
            own("arr", 24, 32),
            own("l", 56, 8),
            own("w", 64, 4),
            own("p", 72, 8),
            own("bits:3, more:7", 80, 4),
            own("u", 84, 4),
            own("ld", 96, 16),
        ]
    );
    let derived = render_record(recs.iter().find(|r| r.name == "Derived").unwrap(), 64);
    assert!(derived.contains("3 of 24 bytes are padding"), "{derived}");
}

// ---------------- cacheline straddle ----------------

#[test]
fn detects_cacheline_straddle() {
    // A member at offset 60, size 8 crosses the 64-byte boundary (60..68).
    let rec = Record {
        name: "Straddler".to_string(),
        size: 72,
        align: Some(8),
        members: vec![
            Member {
                name: "pad".to_string(),
                offset: 0,
                size: Some(60),
            },
            Member {
                name: "v".to_string(),
                offset: 60,
                size: Some(8),
            },
        ],
        source: "pahole (exact)",
    };
    let out = render_record(&rec, 64);
    assert!(
        out.contains("member 'v' spans cachelines 0-1 (offset 60..68)"),
        "{out}"
    );
}

// ---------------- pahole ----------------

const PAHOLE: &str = "\
struct Bad {
	char                       a;                    /*     0     1 */

	/* XXX 7 bytes hole, try to pack */

	double                     b;                    /*     8     8 */
	char                       c;                    /*    16     1 */

	/* XXX 3 bytes hole, try to pack */

	int                        d;                    /*    20     4 */
	char                       e;                    /*    24     1 */

	/* size: 32, cachelines: 1, members: 5 */
	/* sum members: 20, holes: 2, sum holes: 10 */
	/* padding: 7 */
};
";

#[test]
fn parses_pahole_members_and_size() {
    let recs = parse_pahole(PAHOLE);
    assert_eq!(recs.len(), 1);
    let bad = &recs[0];
    assert_eq!(bad.name, "Bad");
    assert_eq!(bad.size, 32);
    assert_eq!(bad.source, "pahole (exact)");
    // 5 members; summary comment lines must NOT be parsed as members.
    assert_eq!(bad.members.len(), 5);
    assert_eq!(
        bad.members[0],
        Member {
            name: "a".to_string(),
            offset: 0,
            size: Some(1)
        }
    );
    assert_eq!(
        bad.members[3],
        Member {
            name: "d".to_string(),
            offset: 20,
            size: Some(4)
        }
    );
}

#[test]
fn render_pahole_is_exact_not_partial() {
    let recs = parse_pahole(PAHOLE);
    let out = render_record(&recs[0], 64);
    assert!(out.contains("(source: pahole (exact))"));
    assert!(out.contains("-- 7-byte hole --"));
    assert!(out.contains("bytes are padding"));
    assert!(!out.contains("partial"));
}

// ---------------- compile_commands.json extraction ----------------

#[test]
fn compile_db_flags_extracts_includes_and_defines() {
    let dir = std::env::temp_dir().join(format!("byteask-cdb-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let db = r#"[
      {"directory":"/x","file":"/x/src/foo.cpp",
       "arguments":["clang++","-c","-Iinclude","-DFOO=1","-std=c++20","-o","foo.o","foo.cpp","-isystem","/opt/inc"]}
    ]"#;
    std::fs::write(dir.join("compile_commands.json"), db).unwrap();
    let flags = compile_db_flags(&dir, "foo.cpp");
    // Relative include dirs resolve against the entry's `directory`.
    assert!(flags.contains(&"-I/x/include".to_string()));
    assert!(flags.contains(&"-DFOO=1".to_string()));
    assert!(flags.contains(&"-std=c++20".to_string()));
    assert!(flags.contains(&"-isystem".to_string()));
    assert!(flags.contains(&"/opt/inc".to_string()));
    // Output/compile-action flags are dropped.
    assert!(!flags.iter().any(|f| f == "-o" || f == "foo.o" || f == "-c"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compile_db_flags_survive_cmake_quoting_and_out_of_source_builds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.cpp"), "").unwrap();
    let db = serde_json::json!([{
        "directory": root.join("build"),
        "file": root.join("src/main.cpp"),
        "command": "/usr/bin/c++ -DVERSION=\\\"1.0\\\" \"-I/p/my inc\" -I../include -I gen \
                    -D MODE=2 -m32 -std=c++17 -o main.o -c ../src/main.cpp",
    }]);
    std::fs::write(root.join("build/compile_commands.json"), db.to_string()).unwrap();
    let flags = compile_db_flags(root, "src/main.cpp");
    let build = root.join("build");
    assert_eq!(
        flags,
        vec![
            "-DVERSION=\"1.0\"".to_string(),
            "-I/p/my inc".to_string(),
            format!("-I{}", build.join("../include").display()),
            "-I".to_string(),
            build.join("gen").display().to_string(),
            "-D".to_string(),
            "MODE=2".to_string(),
            "-m32".to_string(),
            "-std=c++17".to_string(),
        ]
    );
}

#[test]
fn compile_db_flags_never_borrow_another_files_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in ["a", "b"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
        std::fs::write(root.join(sub).join("util.cpp"), "").unwrap();
    }
    let db = serde_json::json!([
        {"directory": root, "file": root.join("a/util.cpp"), "command": "c++ -DA_ONLY -c a/util.cpp"},
        {"directory": root, "file": root.join("b/util.cpp"), "command": "c++ -DB_ONLY -c b/util.cpp"},
    ]);
    std::fs::write(root.join("compile_commands.json"), db.to_string()).unwrap();
    assert_eq!(
        compile_db_flags(root, "b/util.cpp"),
        vec!["-DB_ONLY".to_string()]
    );
    // Not in the database, and its name is ambiguous: no flags rather than a guess.
    std::fs::create_dir_all(root.join("c")).unwrap();
    std::fs::write(root.join("c/util.cpp"), "").unwrap();
    assert!(compile_db_flags(root, "c/util.cpp").is_empty());
}

#[test]
fn compile_db_flags_empty_when_no_db() {
    let dir = std::env::temp_dir().join(format!("byteask-nocdb-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    assert!(compile_db_flags(&dir, "foo.cpp").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- messages ----------------

#[test]
fn no_match_message_asks_for_a_completing_tu() {
    let args = LayoutArgs {
        file: "types.h".to_string(),
        name: Some("Order".to_string()),
        flags: None,
        cacheline: None,
    };
    let msg = no_match_message(&args, None);
    assert!(msg.contains("Order"));
    assert!(msg.contains("does NOT synthesize an include"));
}

#[test]
fn unavailable_message_names_backends() {
    let p = Probe::NotInstalled {
        tool: "pahole".to_string(),
        install_hint: "apt install dwarves".to_string(),
    };
    let msg = unavailable_message(&p);
    assert!(msg.contains("clang"));
    assert!(msg.contains("dwarves"));
    assert!(msg.contains("cpp_godbolt"));
}

// ---------------- live ----------------

#[tokio::test]
#[ignore = "needs clang; run with --include-ignored"]
async fn live_clang_layout_reports_bad_struct() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("lay.cpp");
    std::fs::write(
        &src,
        "struct Bad { char a; double b; char c; int d; char e; };\nBad g;\n",
    )
    .expect("write");
    let tool = CppLayoutTool {
        cwd: dir.path().to_path_buf(),
    };
    let call_args = LayoutArgs {
        file: "lay.cpp".to_string(),
        name: Some("Bad".to_string()),
        flags: None,
        cacheline: None,
    };
    let Some(clang) = clang_binary(dir.path()).await else {
        eprintln!("skip: no clang");
        return;
    };
    let file = dir.path().join("lay.cpp").display().to_string();
    let outcome = tool
        .clang_layout(
            &clang,
            &call_args,
            &file,
            "c++",
            &[],
            64,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("clang layout");
    match outcome {
        ClangOutcome::Records(text) => {
            assert!(text.contains("Bad: size 32"), "{text}");
            assert!(text.contains("-- 7-byte hole --"), "{text}");
            assert!(text.contains("bytes are padding"), "{text}");
        }
        _ => panic!("expected records"),
    }
}

// ---------------- pahole: nested aggregates, arrays, bitfields ----------------

/// Verbatim pahole 1.24 output for a struct holding every declarator shape that
/// used to break the parser (bitfields, a function pointer, a named and an
/// anonymous inline union, an inline struct, an array).
const PAHOLE_KINDS: &str = "struct Kinds {
\tunsigned int               a:3;                  /*     0: 0  4 */
\tunsigned int               b:5;                  /*     0: 3  4 */

\t/* XXX 24 bits hole, try to pack */
\t/* XXX 4 bytes hole, try to pack */

\tvoid                       (*cb)(int);           /*     8     8 */
\tunion {
\t\tint                i;                    /*    16     4 */
\t\tfloat              f;                    /*    16     4 */
\t} u;                                             /*    16     4 */
\tunion {
\t\tint                        i;                    /*     0     4 */
\t\tfloat                      f;                    /*     0     4 */
\t};

\tstruct {
\t\tchar               x;                    /*    20     1 */
\t} anon_s;                                        /*    20     1 */

\t/* XXX 3 bytes hole, try to pack */
\tstruct {
\t\tchar                       x;                    /*     0     1 */

\t\t/* size: 1, cachelines: 1, members: 1 */
\t\t/* last cacheline: 1 bytes */
\t};


\tunion {
\t\tlong               l;                    /*    24     8 */
\t\tdouble             dd;                   /*    24     8 */
\t};                                               /*    24     8 */
\tunion {
\t\tlong                       l;                    /*     0     8 */
\t\tdouble                     dd;                   /*     0     8 */
\t};

\tchar                       tail[3];              /*    32     3 */

\t/* size: 40, cachelines: 1, members: 7 */
\t/* sum members: 24, holes: 2, sum holes: 7 */
\t/* sum bitfield members: 8 bits, bit holes: 1, sum bit holes: 24 bits */
\t/* padding: 5 */
\t/* last cacheline: 40 bytes */
};
";

#[test]
fn pahole_nested_aggregates_are_one_member_and_never_a_new_record() {
    let recs = parse_pahole(PAHOLE_KINDS);
    assert_eq!(recs.len(), 1, "{recs:#?}");
    let rec = &recs[0];
    assert_eq!(rec.name, "Kinds");
    assert_eq!(rec.size, 40);
    let members: Vec<(&str, u64, Option<u64>)> = rec
        .members
        .iter()
        .map(|m| (m.name.as_str(), m.offset, m.size))
        .collect();
    assert_eq!(
        members,
        vec![
            ("a:3, b:5", 0, Some(4)),
            ("cb", 8, Some(8)),
            ("u", 16, Some(4)),
            ("anon_s", 20, Some(1)),
            ("(anonymous)", 24, Some(8)),
            ("tail", 32, Some(3)),
        ]
    );
    // pahole's own summary: 7 bytes of holes + 5 of tail padding.
    let out = render_record(rec, 64);
    assert!(out.contains("12 of 40 bytes are padding (30%)"), "{out}");
    assert!(
        out.contains("-- 4-byte hole --") && out.contains("-- 3-byte hole --"),
        "{out}"
    );
}

#[test]
fn pahole_member_names_understand_every_declarator() {
    let name = |decl: &str| pahole_member_name(&format!("{decl};  /* 0 4 */"));
    assert_eq!(
        name("char                       tag[6]").as_deref(),
        Some("tag")
    );
    assert_eq!(
        name("int                        grid[2][3]").as_deref(),
        Some("grid")
    );
    assert_eq!(
        name("char *                     names[4]").as_deref(),
        Some("names")
    );
    assert_eq!(
        name("void                       (*cb)(int)").as_deref(),
        Some("cb")
    );
    assert_eq!(
        name("int                        (**_vptr.Foo)(void)").as_deref(),
        Some("_vptr.Foo")
    );
    assert_eq!(
        name("std::uint8_t               flag:1").as_deref(),
        Some("flag:1")
    );
    assert_eq!(
        name("class Base                 <ancestor>").as_deref(),
        Some("(base Base)")
    );
    assert_eq!(name("std::string                s").as_deref(), Some("s"));
}

/// Every record's padding must equal what pahole itself sums (`sum holes` +
/// `padding`), across declarator shapes: the tool is labelled "exact".
#[test]
fn real_pahole_padding_matches_paholes_own_summary() {
    use crate::program_args::fixture::have;
    if !have("pahole") || !have("c++") {
        eprintln!("skip: needs pahole and c++");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("kinds.cpp");
    std::fs::write(
        &src,
        "#include <cstdint>\n#include <string>\n\
         struct Arr { char tag[6]; int n; };\n\
         struct Multi { int grid[2][3]; char c; double d; };\n\
         struct Ptrs { char *names[4]; short s; };\n\
         struct Bits { unsigned a:3; unsigned b:5; std::uint8_t c:1; double d; };\n\
         struct Nest { char c; union { int i; float f; } u; struct { char x; } s; union { long l; double dd; }; char t[3]; };\n\
         struct Fn { void (*cb)(int); char c; };\n\
         struct Base { int b; virtual ~Base(); };\n\
         struct Derived : Base { char d; long e; };\n\
         struct alignas(64) Hot { std::uint64_t count; char pad[56]; };\n\
         struct Str { char c; std::string s; bool f; };\n\
         Base::~Base() {}\n\
         Arr a; Multi m; Ptrs p; Bits b; Nest n; Fn f; Derived d; Hot h; Str s;\n",
    )
    .unwrap();
    let obj = dir.path().join("kinds.o");
    let built = std::process::Command::new("c++")
        .args(["-g", "-c"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap();
    assert!(built.success());
    let names = [
        "Arr", "Multi", "Ptrs", "Bits", "Nest", "Fn", "Derived", "Hot", "Str",
    ];
    let out = std::process::Command::new("pahole")
        .arg("-C")
        .arg(names.join(","))
        .arg(&obj)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let recs = parse_pahole(&text);
    let parsed: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
    for name in names {
        assert!(
            parsed.contains(&name),
            "{name} missing from {parsed:?}\n{text}"
        );
    }
    // Ground truth where pahole contradicts itself: Derived's `d` sits in
    // Base's tail padding (vptr 8 + int 4, so bytes 12..16), leaving 13..16.
    let derived = recs.iter().find(|r| r.name == "Derived").unwrap();
    let ours = render_record(derived, 64);
    assert!(ours.contains("3 of 24 bytes are padding"), "{ours}");
    assert!(ours.contains("(base Base)"), "{ours}");
    // pahole's summary per record, read straight from its own text.
    // A record ends at a `}` in column 0 (`};` or `} __attribute__(...);`).
    for block in text.split("\n}").filter(|b| !b.trim().is_empty()) {
        let name = block.trim_start().split_whitespace().nth(1).unwrap_or("");
        let Some(rec) = recs.iter().find(|r| r.name == name) else {
            continue;
        };
        if block.contains("BRAIN FART ALERT") {
            // pahole's own sums do not add up for this record (asserted above).
            continue;
        }
        let field = |key: &str| {
            block
                .lines()
                .filter(|l| !l.starts_with("\t\t"))
                .find_map(|l| l.split(key).nth(1))
                .and_then(|rest| {
                    rest.trim()
                        .split(|c: char| !c.is_ascii_digit())
                        .next()
                        .map(str::to_string)
                })
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let pahole_padding = field("sum holes:") + field("padding:");
        let ours = render_record(rec, 64);
        assert!(
            ours.contains(&format!(
                "{pahole_padding} of {} bytes are padding",
                rec.size
            )),
            "{name}: pahole says {pahole_padding} bytes of padding\n--- ours ---\n{ours}\n--- pahole ---\n{block}"
        );
    }
}

#[test]
fn a_misnamed_field_is_an_error_not_silently_ignored() {
    let err = serde_json::from_str::<LayoutArgs>(r#"{"file":"a.cpp","struct":"Order"}"#)
        .err()
        .expect("unknown field refused");
    assert!(err.to_string().contains("unknown field `struct`"), "{err}");
}

// ---------------- both backends, headers, namespaces ----------------

/// Compile nothing: build a tool rooted at `dir`.
fn tool_in(dir: &std::path::Path) -> CppLayoutTool {
    CppLayoutTool {
        cwd: dir.to_path_buf(),
    }
}

fn named(file: &str, name: &str) -> LayoutArgs {
    LayoutArgs {
        file: file.to_string(),
        name: Some(name.to_string()),
        flags: None,
        cacheline: None,
    }
}

fn padding_line(text: &str) -> String {
    text.lines()
        .find(|line| line.contains("bytes are padding") || line.contains("hole analysis: partial"))
        .unwrap_or("(none)")
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The clang backend (the only one on macOS and Windows) must report the same
/// padding as pahole for every declarator shape. Before, it skipped the vptr,
/// attributed a base's and a nested struct's fields to the outer record, and
/// could not size a bitfield or a record-typed member.
#[tokio::test]
async fn real_clang_backend_agrees_with_pahole_on_every_shape() {
    use crate::program_args::fixture::have;
    if !have("pahole") || !have("clang++") {
        eprintln!("skip: needs pahole and clang++");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("shapes.cpp"),
        "#include <cstdint>\n#include <string>\n\
         struct Inner { char c; double d; };\n\
         struct Arr { char tag[6]; int n; };\n\
         struct Ptrs { char *names[4]; short s; };\n\
         struct Bits { unsigned a:3; unsigned b:5; std::uint8_t c:1; double d; };\n\
         struct Nest { char c; union { int i; float f; } u; struct { char x; } s; union { long l; double dd; }; char t[3]; };\n\
         struct Fn { void (*cb)(int); char c; };\n\
         struct Base { int b; virtual ~Base(); };\n\
         struct Derived : Base { char d; long e; };\n\
         struct Empty {};\n\
         struct EBO : Empty { char x; int y; };\n\
         struct NUA { [[no_unique_address]] Empty e; int x; };\n\
         struct alignas(64) Hot { std::uint64_t count; char pad[40]; };\n\
         struct Str { char c; std::string s; bool f; };\n\
         struct Mix { char c; Inner in; Inner arr[2]; long l; wchar_t w; void *p; long double ld; };\n\
         Base::~Base() {}\n\
         Arr a; Ptrs p; Bits b; Nest n; Fn f; Derived d; EBO e; NUA u; Hot h; Str s; Mix m;\n",
    )
    .unwrap();
    let tool = tool_in(dir.path());
    let file = dir.path().join("shapes.cpp").display().to_string();
    let clang = clang_binary(dir.path()).await.expect("clang");
    let timeout = std::time::Duration::from_secs(120);
    for name in [
        "Arr", "Ptrs", "Bits", "Nest", "Fn", "Base", "Derived", "EBO", "NUA", "Hot", "Str", "Mix",
    ] {
        let args = named("shapes.cpp", name);
        let PaholeOutcome::Text(pahole) = tool
            .pahole_layout(&args, &file, "c++", &[], 64, timeout)
            .await
            .unwrap()
        else {
            panic!("pahole gave no layout for {name}");
        };
        let ClangOutcome::Records(clang_text) = tool
            .clang_layout(&clang, &args, &file, "c++", &[], 64, timeout)
            .await
            .unwrap()
        else {
            panic!("clang gave no layout for {name}");
        };
        // The clang output may carry other records matching the substring.
        let clang_record = clang_text
            .split("\n\n")
            .find(|block| {
                block.starts_with(&format!("{name}: ")) || block.contains(&format!("\n{name}: "))
            })
            .unwrap_or(&clang_text);
        assert_eq!(
            padding_line(clang_record),
            padding_line(&pahole),
            "{name}\n--- clang ---\n{clang_record}\n--- pahole ---\n{pahole}"
        );
        assert!(
            !clang_record.contains("partial"),
            "{name} not fully sized:\n{clang_record}"
        );
    }
}

/// A header's own types are unused in its own translation unit. Given to
/// `-c`, the header became a precompiled header and pahole saw nothing; that
/// was 11 of 24 real calls. And pahole shows one record per unqualified name.
#[tokio::test]
async fn real_pahole_reads_a_headers_types_and_says_which_namespace() {
    use crate::program_args::fixture::have;
    if !have("pahole") || !have("clang++") || !have("readelf") {
        eprintln!("skip: needs pahole, clang++ and readelf");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("cache.hpp"),
        "#pragma once\n#include <cstdint>\n\
         namespace kvcache { struct Entry { std::uint32_t key; double value; bool live; }; }\n\
         namespace other { struct Entry { char c; }; }\n\
         typedef struct { int a; char b; } CTypedef;\n",
    )
    .unwrap();
    let tool = tool_in(dir.path());
    let file = dir.path().join("cache.hpp").display().to_string();
    let timeout = std::time::Duration::from_secs(120);
    let run = |name: &'static str| {
        let args = named("cache.hpp", name);
        let tool = &tool;
        let file = file.clone();
        async move {
            tool.pahole_layout(&args, &file, "c++", &[], 64, timeout)
                .await
                .unwrap()
        }
    };
    match run("kvcache::Entry").await {
        PaholeOutcome::Text(text) => assert!(text.contains("kvcache::Entry: size 24"), "{text}"),
        PaholeOutcome::Declined(why) => panic!("declined: {why:?}"),
    }
    match run("Entry").await {
        PaholeOutcome::Text(text) => {
            assert!(text.contains("2 records are named Entry"), "{text}");
            assert!(
                text.contains("kvcache::Entry (24 B)") && text.contains("other::Entry (1 B)"),
                "{text}"
            );
        }
        PaholeOutcome::Declined(why) => panic!("declined: {why:?}"),
    }
    // pahole shows kvcache::Entry for `-C Entry`; asked for the other one it
    // must decline rather than present the wrong record.
    match run("other::Entry").await {
        PaholeOutcome::Declined(Some(why)) => assert!(why.contains("other::Entry"), "{why}"),
        PaholeOutcome::Text(text) => {
            assert!(
                text.contains("other::Entry: size 1"),
                "wrong record presented:\n{text}"
            )
        }
        PaholeOutcome::Declined(None) => panic!("declined without saying why"),
    }
    match run("CTypedef").await {
        PaholeOutcome::Text(text) => assert!(text.contains("CTypedef: size 8"), "{text}"),
        PaholeOutcome::Declined(why) => panic!("declined: {why:?}"),
    }
}

#[test]
fn dwarf_records_carry_their_namespace() {
    let dump = "\
 <1><41f>: Abbrev Number: 2 (DW_TAG_namespace)
    <420>   DW_AT_name        : (indexed string: 0x6e): kvcache
 <2><421>: Abbrev Number: 15 (DW_TAG_structure_type)
    <423>   DW_AT_name        : (indexed string: 0x74): Entry
    <424>   DW_AT_byte_size   : 24
 <3><427>: Abbrev Number: 9 (DW_TAG_member)
    <428>   DW_AT_name        : (indexed string: 0x75): key
 <1><44c>: Abbrev Number: 2 (DW_TAG_namespace)
    <44d>   DW_AT_name        : (indexed string: 0x75): other
 <2><44e>: Abbrev Number: 15 (DW_TAG_structure_type)
    <450>   DW_AT_name        : (indexed string: 0x74): Entry
    <451>   DW_AT_byte_size   : 1
 <2><460>: Abbrev Number: 16 (DW_TAG_structure_type)
    <461>   DW_AT_name        : Entry
    <462>   DW_AT_declaration : 1
 <1><470>: Abbrev Number: 15 (DW_TAG_structure_type)
    <471>   DW_AT_name        : Entry
    <472>   DW_AT_byte_size   : 4
";
    assert_eq!(
        parse_dwarf_records(dump, "Entry"),
        vec![
            ("kvcache::Entry".to_string(), 24),
            ("other::Entry".to_string(), 1),
            ("Entry".to_string(), 4),
        ]
    );
}

#[test]
fn qualify_labels_decline_or_note() {
    let rec = |size: u64| Record {
        name: "Entry".to_string(),
        size,
        align: None,
        members: Vec::new(),
        source: "pahole (exact)",
    };
    let defs = vec![
        ("kvcache::Entry".to_string(), 24),
        ("other::Entry".to_string(), 1),
    ];
    // Asked by qualified name, and pahole's record is it.
    let mut records = vec![rec(24)];
    assert!(matches!(
        qualify(&mut records, "kvcache::Entry", &defs),
        Qualified::Labelled(None)
    ));
    assert_eq!(records[0].name, "kvcache::Entry");
    // Asked for the other one: pahole's record is not it.
    let mut records = vec![rec(24)];
    assert!(matches!(
        qualify(&mut records, "other::Entry", &defs),
        Qualified::NotIt(_)
    ));
    // Asked unqualified: identified by size, with a note naming both.
    let mut records = vec![rec(1)];
    match qualify(&mut records, "Entry", &defs) {
        Qualified::Labelled(Some(note)) => assert!(note.contains("this is other::Entry"), "{note}"),
        _ => panic!("expected a note"),
    }
    // Same size: cannot tell which, and says so.
    let tied = vec![("a::Entry".to_string(), 8), ("b::Entry".to_string(), 8)];
    let mut records = vec![rec(8)];
    match qualify(&mut records, "Entry", &tied) {
        Qualified::Labelled(Some(note)) => assert!(note.contains("cannot be told"), "{note}"),
        _ => panic!("expected a note"),
    }
    assert_eq!(records[0].name, "Entry");
}

#[test]
fn language_follows_the_extension() {
    assert_eq!(language("x.c"), "c");
    for file in ["x.cpp", "x.cc", "x.h", "x.hpp", "dir.c/x.cpp"] {
        assert_eq!(language(file), "c++", "{file}");
    }
}

/// `name` is documented as a substring filter; pahole's `-C` matches whole
/// names, so "Order" never found OrderBook in a header (and the clang fallback
/// cannot see a header's unused types).
#[tokio::test]
async fn real_pahole_honours_a_substring_name_in_a_header() {
    use crate::program_args::fixture::have;
    if !have("pahole") || !have("clang++") || !have("readelf") {
        eprintln!("skip: needs pahole, clang++ and readelf");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("book.h"),
        "#pragma once\n#include <string>\n\
         namespace mkt { struct OrderBook { char side; double px; }; }\n\
         struct OrderRec { int id; char kind; };\n",
    )
    .unwrap();
    let tool = tool_in(dir.path());
    let file = dir.path().join("book.h").display().to_string();
    let timeout = std::time::Duration::from_secs(120);
    let PaholeOutcome::Text(text) = tool
        .pahole_layout(&named("book.h", "Order"), &file, "c++", &[], 64, timeout)
        .await
        .unwrap()
    else {
        panic!("a substring of two record names found nothing");
    };
    assert!(text.contains("mkt::OrderBook: size 16"), "{text}");
    assert!(text.contains("OrderRec: size 8"), "{text}");
}

/// Without a name the report is about the file's records, not every type of
/// every header it includes.
#[tokio::test]
async fn real_whole_file_listing_is_not_flooded_by_system_headers() {
    use crate::program_args::fixture::have;
    if !have("pahole") || !have("clang++") {
        eprintln!("skip: needs pahole and clang++");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("net.cpp"),
        // C headers declare many global-namespace structs (pahole's full
        // listing omits namespaced ones, so <string> alone would not show it).
        "#include <sys/socket.h>\n#include <sys/stat.h>\n#include <netinet/in.h>\n#include <time.h>\n\
         #include <string>\nstruct Conn { int fd; std::string peer; };\nConn c;\n",
    )
    .unwrap();
    let tool = tool_in(dir.path());
    let file = dir.path().join("net.cpp").display().to_string();
    let args = LayoutArgs {
        file: "net.cpp".to_string(),
        name: None,
        flags: None,
        cacheline: None,
    };
    let PaholeOutcome::Text(text) = tool
        .pahole_layout(
            &args,
            &file,
            "c++",
            &[],
            64,
            std::time::Duration::from_secs(120),
        )
        .await
        .unwrap()
    else {
        panic!("no layout");
    };
    let records = text.matches("(source: pahole").count();
    assert!(text.contains("Conn: size"), "{text}");
    assert!(
        records < 20,
        "{records} records listed for a file declaring one:\n{text}"
    );
}
