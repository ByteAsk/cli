use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;

// ------------------------------ ELF notes ------------------------------

fn note(kind: u32, name: &[u8], desc: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((name.len() as u32).to_le_bytes());
    out.extend((desc.len() as u32).to_le_bytes());
    out.extend(kind.to_le_bytes());
    out.extend(name);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend(desc);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn prstatus(tid: u32, cursig: u16, rip: u64, rsp: u64) -> Vec<u8> {
    let mut d = vec![0u8; 112 + 27 * 8 + 8];
    d[12..14].copy_from_slice(&cursig.to_le_bytes());
    d[32..36].copy_from_slice(&tid.to_le_bytes());
    d[112 + 16 * 8..112 + 17 * 8].copy_from_slice(&rip.to_le_bytes());
    d[112 + 19 * 8..112 + 20 * 8].copy_from_slice(&rsp.to_le_bytes());
    d
}

fn siginfo(signo: i32, code: i32, word: u64) -> Vec<u8> {
    let mut d = vec![0u8; 128];
    d[0..4].copy_from_slice(&signo.to_le_bytes());
    d[8..12].copy_from_slice(&code.to_le_bytes());
    d[16..24].copy_from_slice(&word.to_le_bytes());
    d
}

fn prpsinfo(pid: u32, fname: &str, args: &str) -> Vec<u8> {
    let mut d = vec![0u8; 136];
    d[24..28].copy_from_slice(&pid.to_le_bytes());
    d[40..40 + fname.len()].copy_from_slice(fname.as_bytes());
    d[56..56 + args.len()].copy_from_slice(args.as_bytes());
    d
}

fn nt_file(entries: &[(u64, u64, &str)]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend((entries.len() as u64).to_le_bytes());
    d.extend(4096u64.to_le_bytes());
    for (s, e, _) in entries {
        d.extend(s.to_le_bytes());
        d.extend(e.to_le_bytes());
        d.extend(0u64.to_le_bytes());
    }
    for (_, _, p) in entries {
        d.extend(p.as_bytes());
        d.push(0);
    }
    d
}

/// A minimal x86-64 core: one PT_NOTE plus PT_LOADs `(vaddr, size, flags)`.
fn synthetic_core(notes: &[u8], loads: &[(u64, u64, u32)]) -> Vec<u8> {
    let phnum = 1 + loads.len();
    let phoff = 64u64;
    let notes_at = phoff + 56 * phnum as u64;
    let mut out = vec![0u8; 64];
    out[0..4].copy_from_slice(b"\x7fELF");
    out[4] = 2;
    out[5] = 1;
    out[16..18].copy_from_slice(&4u16.to_le_bytes());
    out[18..20].copy_from_slice(&62u16.to_le_bytes());
    out[32..40].copy_from_slice(&phoff.to_le_bytes());
    out[54..56].copy_from_slice(&56u16.to_le_bytes());
    out[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
    let mut ph = |p_type: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64| {
        let mut h = vec![0u8; 56];
        h[0..4].copy_from_slice(&p_type.to_le_bytes());
        h[4..8].copy_from_slice(&flags.to_le_bytes());
        h[8..16].copy_from_slice(&offset.to_le_bytes());
        h[16..24].copy_from_slice(&vaddr.to_le_bytes());
        h[32..40].copy_from_slice(&filesz.to_le_bytes());
        h[40..48].copy_from_slice(&memsz.to_le_bytes());
        out.extend(h);
    };
    ph(4, 0, notes_at, 0, notes.len() as u64, 0);
    for (vaddr, size, flags) in loads {
        ph(1, *flags, 0, *vaddr, 0, *size);
    }
    out.extend(notes);
    out
}

#[test]
fn the_notes_give_the_signal_the_faulting_thread_and_the_memory_map() {
    let mut notes = Vec::new();
    notes.extend(note(1, b"CORE\0", &prstatus(4242, 11, 0x5555_0000_1135, 0x7ffd_0000_0c50)));
    notes.extend(note(3, b"CORE\0", &prpsinfo(4240, "trader", "./trader --cfg prod.toml")));
    notes.extend(note(0x5349_4749, b"CORE\0", &siginfo(11, 1, 0x18)));
    notes.extend(note(0x4649_4c45, b"CORE\0", &nt_file(&[(0x5555_0000_0000, 0x5555_0000_2000, "/opt/app/trader"), (0x7f00_0000_0000, 0x7f00_0000_4000, "/data/journal.bin")])));
    notes.extend(note(1, b"CORE\0", &prstatus(4243, 0, 0, 0)));
    let core = synthetic_core(&notes, &[(0x5555_0000_0000, 0x2000, 5), (0x7f00_0000_0000, 0x4000, 6), (0x7ffd_0000_0000, 0x21000, 6)]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("core");
    std::fs::write(&path, core).unwrap();
    let n = read_core_notes(&path).unwrap();
    assert_eq!(n.threads, vec![4242, 4243]);
    assert_eq!((n.signo, n.code, n.addr), (Some(11), Some(1), Some(0x18)));
    assert_eq!(n.cursig, Some(11));
    assert_eq!((n.pc, n.sp), (Some(0x5555_0000_1135), Some(0x7ffd_0000_0c50)));
    assert_eq!(n.pid, Some(4240));
    assert_eq!(n.fname.as_deref(), Some("trader"));
    assert_eq!(n.args.as_deref(), Some("./trader --cfg prod.toml"));
    assert_eq!(n.maps.len(), 3);
    assert_eq!(n.maps[0].file.as_deref(), Some("/opt/app/trader"));
    assert_eq!(n.maps[0].perms(), "r-x");
    assert_eq!(n.maps[1].file.as_deref(), Some("/data/journal.bin"));
    assert_eq!(n.maps[2].file, None);
    assert_eq!(n.maps[2].perms(), "rw-");
}

#[test]
fn a_kill_signal_records_its_sender_not_an_address() {
    let mut notes = note(1, b"CORE\0", &prstatus(7, 6, 1, 1));
    notes.extend(note(0x5349_4749, b"CORE\0", &siginfo(6, 0, 999)));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("core");
    std::fs::write(&path, synthetic_core(&notes, &[])).unwrap();
    let n = read_core_notes(&path).unwrap();
    assert_eq!(n.sender_pid, Some(999));
    assert_eq!(n.addr, None);
}

#[test]
fn a_file_that_is_not_a_core_is_an_error_never_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("empty", Vec::new()),
        ("text", b"hello world, not an ELF file at all; long enough to read a header from".repeat(2)),
        ("truncated", synthetic_core(&note(1, b"CORE\0", &prstatus(1, 11, 1, 1)), &[])[..80].to_vec()),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        assert!(read_core_notes(&path).is_err(), "{name}");
    }
    // An executable is ELF but not a core.
    let mut exe = synthetic_core(&[], &[]);
    exe[16..18].copy_from_slice(&2u16.to_le_bytes());
    let path = dir.path().join("exe");
    std::fs::write(&path, exe).unwrap();
    assert!(read_core_notes(&path).unwrap_err().contains("not a core dump"));
}

#[test]
fn the_program_is_found_from_the_core_when_it_exists_here() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("trader");
    std::fs::write(&exe, b"x").unwrap();
    let notes = CoreNotes {
        fname: Some("trader".into()),
        maps: vec![
            Mapping { start: 0x7f00, end: 0x8000, read: true, write: false, exec: true, file: Some("/lib/libc.so.6".into()) },
            Mapping { start: 0x5000, end: 0x6000, read: true, write: false, exec: true, file: Some(exe.to_string_lossy().into_owned()) },
        ],
        ..CoreNotes::default()
    };
    assert_eq!(program_from_core(&notes, None), Some(exe.clone()));
    // From another machine: found under the sysroot by its original path.
    let root = tempfile::tempdir().unwrap();
    let remote = CoreNotes {
        fname: Some("trader".into()),
        maps: vec![Mapping { start: 0x5000, end: 0x6000, read: true, write: false, exec: true, file: Some("/opt/app/trader".into()) }],
        ..CoreNotes::default()
    };
    assert_eq!(program_from_core(&remote, None), None);
    std::fs::create_dir_all(root.path().join("opt/app")).unwrap();
    std::fs::write(root.path().join("opt/app/trader"), b"x").unwrap();
    assert_eq!(program_from_core(&remote, Some(root.path())), Some(root.path().join("opt/app/trader")));
}

#[test]
fn the_memory_operand_of_the_faulting_instruction_is_parsed() {
    assert_eq!(memory_base("mov    0x8(%rax),%rax"), Some(("rax".into(), 8)));
    assert_eq!(memory_base("movsd  (%rax),%xmm0"), Some(("rax".into(), 0)));
    assert_eq!(memory_base("movb   $0x58,(%rax)"), Some(("rax".into(), 0)));
    assert_eq!(memory_base("mov    %rdi,-0x108(%rbp)"), Some(("rbp".into(), -0x108)));
    assert_eq!(memory_base("mov    0x10(%rbx,%rcx,8),%rdx"), Some(("rbx".into(), 0x10)));
    assert_eq!(memory_base("idivl  -0x8(%rbp)"), Some(("rbp".into(), -8)));
    assert_eq!(memory_base("ldr x0, [x1, #8]"), Some(("x1".into(), 8)));
    assert_eq!(memory_base("ldr x0, [x19]"), Some(("x19".into(), 0)));
    assert_eq!(memory_base("ud2"), None);
    assert_eq!(memory_base("add    %rax,%rbx"), None);
}

#[test]
fn text_bytes_in_a_pointer_are_recognised() {
    assert_eq!(printable_ascii(0x4141_4141_4141_4141).as_deref(), Some("AAAAAAAA"));
    assert_eq!(printable_ascii(0x0000_7ffd_1234_5678), None);
    assert_eq!(printable_ascii(0), None);
}

#[test]
fn gdbs_argv_is_hardened_and_keeps_debug_info_downloads_off() {
    let argv = build_gdb_argv(Path::new("/w/trader"), Path::new("/w/core"), None, Path::new("/tmp/d.py"));
    assert_eq!(
        argv,
        [
            "-q",
            "-nx",
            "-batch",
            "-iex",
            "set auto-load off",
            "-iex",
            "set debuginfod enabled off",
            "-ex",
            "set may-call-functions off",
            "-ex",
            "source /tmp/d.py",
            "/w/trader",
            "/w/core"
        ]
        .map(str::to_string)
        .to_vec()
    );
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("usr/lib/debug")).unwrap();
    let argv = build_gdb_argv(Path::new("/w/t"), Path::new("/w/c"), Some(root.path()), Path::new("/d.py"));
    assert!(argv.contains(&format!("set sysroot {}", root.path().display())));
    assert!(argv.contains(&format!("set debug-file-directory {}", root.path().join("usr/lib/debug").display())));
}

#[test]
fn without_gdb_the_notes_still_give_a_first_guess_and_say_so() {
    let notes = CoreNotes {
        threads: vec![10],
        signo: Some(11),
        code: Some(1),
        addr: Some(0x8),
        pid: Some(10),
        fname: Some("trader".into()),
        ..CoreNotes::default()
    };
    let text = render(&notes, Path::new("/w/core"), Path::new("/w/trader"), &GdbAnswer::default(), "Python scripting is not supported in this copy of GDB.", None, Path::new("/w"));
    assert!(text.contains("CRASH: SIGSEGV at address 0x8"), "{text}");
    assert!(text.contains("FAULTING THREAD: 10 (thread 1 of 1; the kernel dumped it first)"), "{text}");
    assert!(text.contains("LIKELY CAUSE: a NULL pointer dereference"), "{text}");
    assert!(text.contains("treat it as a first guess"), "{text}");
    assert!(text.contains("backtrace: RESULT: NOT RUN") && text.contains("without Python"), "{text}");
}

// ------------------------------ the cause rules ------------------------------

fn user_frame(function: &str, line: u64) -> Frame {
    Frame { function: Some(function.into()), file: Some(format!("/w/{function}.cpp")), line: Some(line), user: true, ..Frame::default() }
}

fn sys_frame(function: &str) -> Frame {
    Frame { function: Some(function.into()), library: Some("/lib/x86_64-linux-gnu/libc.so.6".into()), ..Frame::default() }
}

fn cause_for(notes: CoreNotes, insn: Option<&str>, regs: &[(&str, &str)], frames: Vec<Frame>) -> Cause {
    cause_with(notes, GdbCore { instruction: insn.map(str::to_string), registers: regs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..GdbCore::default() }, frames)
}

fn cause_with(notes: CoreNotes, gdb: GdbCore, frames: Vec<Frame>) -> Cause {
    let thread = GdbThread { tid: 1, faulting: true, frames, truncated: false };
    let facts = Facts { notes: &notes, gdb: Some(&gdb), faulting: Some(&thread), others: Vec::new() };
    likely_cause(&facts, Path::new("/w"))
}

fn segv(code: i32, addr: u64) -> CoreNotes {
    CoreNotes { signo: Some(11), code: Some(code), addr: Some(addr), pid: Some(1), threads: vec![1], ..CoreNotes::default() }
}

#[test]
fn null_is_decided_by_the_pointer_register_not_the_fault_address() {
    let null = cause_for(segv(1, 0x8), Some("mov    0x8(%rax),%rax"), &[("rax", "0x0")], vec![user_frame("qty_of", 2)]);
    assert!(null.title.starts_with("a NULL pointer dereference in qty_of() at qty_of.cpp:2, reading the member at offset 0x8"), "{null:?}");
    // Found by the live test: address 0x10 from a pointer holding 0x10 was
    // called NULL. It is a bad pointer.
    let small = cause_for(segv(1, 0x10), Some("mov    (%rax),%rax"), &[("rax", "0x10")], vec![user_frame("walk", 2)]);
    assert!(small.title.starts_with("a bad pointer 0x10 — not NULL"), "{small:?}");
    // No instruction known: the address alone decides.
    let unknown = cause_for(segv(1, 0x18), None, &[], vec![user_frame("walk", 2)]);
    assert!(unknown.title.starts_with("a NULL pointer dereference"), "{unknown:?}");
}

#[test]
fn text_over_a_pointer_and_a_jump_to_nowhere_are_named() {
    let text = cause_for(segv(128, 0), Some("movsd  (%rax),%xmm0"), &[("rax", "0x4141414141414141")], vec![user_frame("read", 3)]);
    assert!(text.title.contains("overwritten with text") && text.title.contains("\"AAAAAAAA\""), "{text:?}");
    let mut notes = segv(1, 0xdead0000);
    notes.pc = Some(0xdead0000);
    let jump = cause_for(notes, None, &[], vec![Frame::default(), user_frame("dispatch", 2)]);
    assert!(jump.title.contains("a call or jump to the bad address 0xdead0000"), "{jump:?}");
    assert!(jump.title.contains("the call was made from dispatch() at dispatch.cpp:2"), "{jump:?}");
}

#[test]
fn recursion_into_the_guard_page_is_a_stack_overflow() {
    let mut notes = segv(1, 0x7fff_0000_def8);
    notes.sp = Some(0x7fff_0000_def0);
    let frames = (0..40).map(|_| user_frame("depth", 1)).collect();
    let c = cause_for(notes, Some("mov    %rdi,-0x108(%rbp)"), &[("rbp", "0x7fff0000e000")], frames);
    assert!(c.title.starts_with("a stack overflow: depth recursed"), "{c:?}");
    assert!(c.because.iter().any(|b| b.contains("40 times in a row")), "{c:?}");
}

#[test]
fn read_only_and_mapped_file_faults_use_the_memory_map() {
    let mut notes = segv(2, 0x5000_2004);
    notes.maps = vec![Mapping { start: 0x5000_2000, end: 0x5000_3000, read: true, write: false, exec: false, file: Some("/w/ro".into()) }];
    let ro = cause_for(notes, Some("movb   $0x58,(%rax)"), &[("rax", "0x50002004")], vec![user_frame("upcase", 1)]);
    assert!(ro.title.starts_with("a write to read-only memory in upcase()"), "{ro:?}");
    let bus = cause_for(
        CoreNotes {
            signo: Some(7),
            code: Some(2),
            addr: Some(0x7f00_0000_0008),
            maps: vec![Mapping { start: 0x7f00_0000_0000, end: 0x7f00_0000_2000, read: true, write: true, exec: false, file: Some("/data/journal.bin".into()) }],
            ..CoreNotes::default()
        },
        None,
        &[],
        vec![user_frame("main", 9)],
    );
    assert!(bus.title.contains("past the end of the memory-mapped file /data/journal.bin"), "{bus:?}");
}

#[test]
fn arithmetic_and_illegal_instruction_rules() {
    let mut div_frame = user_frame("per_lot", 1);
    div_frame.args = vec![("qty".into(), "100".into()), ("lots".into(), "0".into())];
    let div = cause_for(CoreNotes { signo: Some(8), code: Some(1), ..CoreNotes::default() }, Some("idivl  -0x8(%rbp)"), &[], vec![div_frame]);
    assert!(div.title.starts_with("an integer division by zero"), "{div:?}");
    assert!(div.because.iter().any(|b| b == "argument `lots` is 0"), "{div:?}");
    let ill = |insn: &str| cause_for(CoreNotes { signo: Some(4), code: Some(2), ..CoreNotes::default() }, Some(insn), &[], vec![user_frame("f", 1)]);
    assert!(ill("ud2").title.starts_with("a trap instruction (ud2)"), "{:?}", ill("ud2"));
    let avx = ill("vpaddd %zmm1,%zmm2,%zmm3");
    assert!(avx.title.contains("this CPU does not support the instruction `vpaddd`"), "{avx:?}");
    assert!(ill("addb   %al,(%rax)").title.starts_with("an illegal instruction"));
}

#[test]
fn abort_rules_read_the_exception_the_message_and_the_stack() {
    let abort = CoreNotes { signo: Some(6), code: Some(-6), sender_pid: Some(1), pid: Some(1), threads: vec![1], ..CoreNotes::default() };
    let gdb = GdbCore {
        exception: Some(Exception { ty: Some("std::runtime_error".into()), what: Some("config missing".into()), frame: 2 }),
        ..GdbCore::default()
    };
    let exc = cause_with(abort.clone(), gdb, vec![sys_frame("abort"), sys_frame("std::terminate()"), sys_frame("__cxa_throw"), user_frame("load", 2)]);
    assert!(exc.title.starts_with("an uncaught C++ exception std::runtime_error thrown from load() at load.cpp:2"), "{exc:?}");
    assert!(exc.because.iter().any(|b| b.contains("\"config missing\"")), "{exc:?}");
    let msg = |text: &str| cause_with(abort.clone(), GdbCore { abort_msg: Some(text.into()), ..GdbCore::default() }, vec![sys_frame("abort"), user_frame("f", 3)]);
    assert!(msg("a.out: a.cpp:2: void f(): Assertion `x' failed.").title.starts_with("an assertion failed in f()"));
    assert!(msg("free(): double free detected in tcache 2").title.starts_with("heap corruption, detected by glibc"));
    assert!(msg("*** stack smashing detected ***: terminated").title.starts_with("a stack buffer overflow in f()"));
    assert!(msg("*** buffer overflow detected ***: terminated").title.starts_with("_FORTIFY_SOURCE caught"));
    let thread = cause_with(abort.clone(), GdbCore::default(), vec![sys_frame("std::terminate()"), sys_frame("std::thread::~thread"), user_frame("main", 2)]);
    assert!(thread.title.starts_with("a std::thread was destroyed while still joinable"), "{thread:?}");
    let killed = cause_with(CoreNotes { sender_pid: Some(777), code: Some(0), ..abort }, GdbCore::default(), vec![sys_frame("pause")]);
    assert!(killed.title.starts_with("killed with SIGABRT by another process (pid 777)"), "{killed:?}");
}

#[test]
fn a_crash_inside_the_allocator_is_reported_as_a_victim() {
    let c = cause_for(segv(1, 0x5555_0000_1000), Some("mov    0x8(%rdx),%rax"), &[("rdx", "0x555500000ff8")], vec![sys_frame("_int_malloc"), sys_frame("malloc"), user_frame("add_order", 7)]);
    assert!(c.title.starts_with("heap corruption: the crash is inside the allocator, called from add_order()"), "{c:?}");
}

// ------------------------- live: real kernel cores -------------------------
//
// Each fixture is compiled, run until it crashes with RLIMIT_CORE raised, and
// the KERNEL's core is triaged by the real tool. Where the kernel does not
// leave the core here (a core_pattern pipe other than apport, systemd-coredump)
// the test skips — or fails with BYTEASK_CORE_LIVE_REQUIRED=1.

fn live_skip(why: &str) {
    if crate::trust::truthy(std::env::var("BYTEASK_CORE_LIVE_REQUIRED").ok().as_deref()) {
        panic!("BYTEASK_CORE_LIVE_REQUIRED is set but: {why}");
    }
    eprintln!("skip: {why}");
}

/// Compile `source` and run it until the kernel dumps its core. Returns the
/// workspace directory, the binary and the core.
fn kernel_core(name: &str, source: &str, flags: &[&str], args: &[&str]) -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;
    if !crate::program_args::fixture::have("g++") || !crate::program_args::fixture::have("gdb") {
        live_skip("g++ or gdb missing");
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let root = dir.path().canonicalize().ok()?;
    std::fs::write(root.join(format!("{name}.cpp")), source).ok()?;
    let built = std::process::Command::new("g++")
        .current_dir(&root)
        .args(["-g", "-O0", "-pthread", "-fno-builtin"])
        .args(flags)
        .args(["-o", name, &format!("{name}.cpp")])
        .status()
        .ok()?
        .success();
    assert!(built, "fixture {name} did not compile");
    let bin = root.join(name);
    let mut cmd = std::process::Command::new(&bin);
    cmd.current_dir(&root).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setrlimit(2) only, in the forked child before exec.
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit { rlim_cur: libc::RLIM_INFINITY, rlim_max: libc::RLIM_INFINITY };
            libc::setrlimit(libc::RLIMIT_CORE, &lim);
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let pid = child.id();
    let status = child.wait().ok()?;
    assert!(status.signal().is_some(), "fixture {name} did not crash: {status:?}");
    // core_pattern "core" / "core.%p", or Ubuntu's apport keeping it under
    // /var/lib/apport/coredump with the pid in the name.
    std::thread::sleep(std::time::Duration::from_millis(300));
    for candidate in [root.join("core"), root.join(format!("core.{pid}"))] {
        if candidate.is_file() {
            return Some((dir, bin, candidate));
        }
    }
    let apport = Path::new("/var/lib/apport/coredump");
    for _ in 0..20 {
        if let Ok(entries) = std::fs::read_dir(apport) {
            let needle = format!(".{pid}.");
            if let Some(found) = entries.flatten().map(|e| e.path()).find(|p| p.to_string_lossy().contains(&needle)) {
                let local = root.join("core");
                if std::fs::copy(&found, &local).is_ok() {
                    return Some((dir, bin, local));
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    live_skip(&format!(
        "the kernel did not leave a core for {name} here (core_pattern: {})",
        std::fs::read_to_string("/proc/sys/kernel/core_pattern").unwrap_or_default().trim()
    ));
    None
}

async fn triage(name: &str, source: &str, flags: &[&str], args: &[&str]) -> Option<String> {
    let (dir, _bin, core) = kernel_core(name, source, flags, args)?;
    let tool = CoreTriageTool { cwd: dir.path().canonicalize().unwrap() };
    let text = crate::test_support::call(&tool, serde_json::json!({ "core": core.to_string_lossy() }))
        .await
        .unwrap_or_else(|e| panic!("debug_core failed on {name}: {e:?}"));
    Some(text)
}

fn cause(text: &str) -> &str {
    text.lines().find(|l| l.starts_with("LIKELY CAUSE: ")).unwrap_or_else(|| panic!("no cause:\n{text}"))
}

#[tokio::test]
async fn live_null_pointer() {
    let Some(text) = triage(
        "null",
        "struct Order { long id; long qty; };\n__attribute__((noinline)) long qty_of(Order *o) { return o->qty; }\nint main() { return (int)qty_of(nullptr); }\n",
        &[],
        &[],
    )
    .await
    else {
        return;
    };
    assert!(text.contains("CRASH: SIGSEGV at address 0x8"), "{text}");
    assert!(text.contains("CRASH SITE (first frame in your code): qty_of() at null.cpp:2"), "{text}");
    assert!(cause(&text).contains("a NULL pointer dereference in qty_of() at null.cpp:2, reading the member at offset 0x8"), "{text}");
    assert!(text.contains("because: faulting instruction: mov") && text.contains("with rax = 0x0"), "{text}");
    assert!(text.contains("the kernel dumped it first"), "{text}");
}

#[tokio::test]
async fn live_small_bad_pointer_and_text_over_a_pointer() {
    if let Some(text) = triage(
        "smallptr",
        "struct Node { long v; Node *next; };\n__attribute__((noinline)) long walk(Node *n) { long t = 0; while (n) { t += n->v; n = n->next; } return t; }\nint main() { Node b{2, (Node *)0x10}; Node a{1, &b}; return (int)walk(&a); }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a bad pointer 0x10 — not NULL, but too small to be a real address"), "{text}");
        assert!(text.contains("NEXT: find who wrote that pointer"), "{text}");
    }
    if let Some(text) = triage(
        "wild",
        "#include <cstring>\nstruct Px { char sym[8]; double *px; };\n__attribute__((noinline)) double read(Px *p) { return *p->px; }\nint main() { Px p; double d = 1; p.px = &d; std::memcpy(p.sym, \"AAAAAAAAAAAAAAAA\", 16); return (int)read(&p); }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a pointer was overwritten with text: 0x4141414141414141 reads as \"AAAAAAAA\""), "{text}");
        assert!(cause(&text).contains("read() at wild.cpp:3"), "{text}");
    }
}

#[tokio::test]
async fn live_abort_family_assert_heap_smash() {
    if let Some(text) = triage(
        "assert",
        "#include <cassert>\n__attribute__((noinline)) void check(int qty) { assert(qty > 0 && \"quantity must be positive\"); }\nint main(int argc, char **) { check(argc - 1); return 0; }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("an assertion failed in check() at assert.cpp:2"), "{text}");
        assert!(text.contains("quantity must be positive"), "{text}");
        assert!(text.contains("args: qty=0"), "{text}");
    }
    if let Some(text) = triage(
        "dfree",
        "#include <cstdlib>\nint main() { char *p = (char *)std::malloc(64); std::free(p); std::free(p); return 0; }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("heap corruption, detected by glibc in main() at dfree.cpp:2"), "{text}");
        assert!(text.contains("double free"), "{text}");
    }
    if let Some(text) = triage(
        "smash",
        "#include <cstring>\n__attribute__((noinline)) void parse(const char *msg) { char sym[8]; std::strcpy(sym, msg); }\nint main() { parse(\"TOO_LONG_SYMBOL_NAME_XXXXXXXX\"); return 0; }\n",
        &["-fstack-protector-strong"],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a stack buffer overflow in parse() at smash.cpp:2"), "{text}");
    }
}

#[tokio::test]
async fn live_stack_overflow_from_recursion() {
    let Some(text) = triage(
        "recurse",
        "__attribute__((noinline)) long depth(long n) { volatile char pad[256]; pad[0] = (char)n; return depth(n + 1) + pad[0]; }\nint main() { return (int)depth(0); }\n",
        &[],
        &[],
    )
    .await
    else {
        return;
    };
    assert!(cause(&text).contains("a stack overflow: depth recursed until the stack ran out"), "{text}");
    assert!(text.contains("times in a row on the stack"), "{text}");
}

#[tokio::test]
async fn live_cxx_terminate_exception_and_joinable_thread() {
    if let Some(text) = triage(
        "throw",
        "#include <stdexcept>\n__attribute__((noinline)) void load(int x) { if (x < 5) throw std::runtime_error(\"config missing\"); }\nint main(int argc, char **) { load(argc); return 0; }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("an uncaught C++ exception std::runtime_error thrown from load() at throw.cpp:2"), "{text}");
        assert!(text.contains("what(): \"config missing\""), "{text}");
    }
    if let Some(text) = triage(
        "joinable",
        "#include <thread>\nint main() { std::thread t([] { for (volatile int i = 0; i < 100000000; i++) {} }); return 0; }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a std::thread was destroyed while still joinable"), "{text}");
        assert!(text.contains("other threads (1)"), "{text}");
    }
}

#[tokio::test]
async fn live_a_worker_threads_exception_names_the_thread_and_its_siblings() {
    let Some(text) = triage(
        "race",
        "#include <thread>\n#include <vector>\nstruct Level { long px; long qty; };\nstd::vector<Level> levels(4);\n\
         __attribute__((noinline)) void apply(int i) {\n  for (long k = 0;; k++) {\n    if (i == 0 && k == 3000000) levels.clear(), levels.shrink_to_fit();\n    Level &l = levels.at(0);\n    l.qty += 1;\n  }\n}\n\
         int main() { std::thread a(apply, 0), b(apply, 1), c(apply, 2); a.join(); b.join(); c.join(); }\n",
        &[],
        &[],
    )
    .await
    else {
        return;
    };
    // Which worker throws first is up to the scheduler; what must hold: the
    // cause names the exception on a std::thread, and the siblings still in
    // apply() are pointed out as a race candidate.
    let c = cause(&text);
    if c.contains("uncaught C++ exception") {
        assert!(c.contains("std::out_of_range") && c.contains("on a std::thread"), "{text}");
        assert!(text.contains("vector::_M_range_check"), "{text}");
    }
    assert!(text.contains("were inside apply at the crash"), "{text}");
    assert!(text.contains("FAULTING THREAD:") && text.contains("of 4"), "{text}");
}

#[tokio::test]
async fn live_arithmetic_trap_and_read_only_write() {
    if let Some(text) = triage(
        "div",
        "__attribute__((noinline)) int per_lot(int qty, int lots) { return qty / lots; }\nint main(int argc, char **) { return per_lot(100, argc - 1); }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("an integer division by zero (or INT_MIN / -1) in per_lot() at div.cpp:1"), "{text}");
        assert!(text.contains("argument `lots` is 0"), "{text}");
    }
    if let Some(text) = triage(
        "trap",
        "__attribute__((noinline)) int side(int s) { if (s == 1) return 1; if (s == 2) return -1; __builtin_trap(); }\nint main(int argc, char **) { return side(argc + 5); }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a trap instruction (ud2) was reached in side() at trap.cpp:1"), "{text}");
    }
    if let Some(text) = triage(
        "ro",
        "__attribute__((noinline)) void upcase(char *s) { s[0] = 'X'; }\nint main() { upcase((char *)\"abc\"); return 0; }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a write to read-only memory in upcase() at ro.cpp:1"), "{text}");
        assert!(text.contains("in a r-- mapping") || text.contains("in a r-x mapping"), "{text}");
    }
}

#[tokio::test]
async fn live_bad_function_pointer_and_truncated_mapped_file() {
    if let Some(text) = triage(
        "fnptr",
        "typedef void (*handler)(int);\n__attribute__((noinline)) void dispatch(handler h) { h(1); }\nint main() { dispatch((handler)0xdead0000); return 0; }\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("a call or jump to the bad address 0xdead0000"), "{text}");
        assert!(cause(&text).contains("the call was made from dispatch() at fnptr.cpp:2"), "{text}");
    }
    if let Some(text) = triage(
        "sigbus",
        "#include <fcntl.h>\n#include <sys/mman.h>\n#include <unistd.h>\nint main() {\n  int fd = open(\"journal.bin\", O_RDWR | O_CREAT | O_TRUNC, 0600);\n  ftruncate(fd, 8192);\n  long *p = (long *)mmap(nullptr, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);\n  ftruncate(fd, 0);\n  return (int)p[1];\n}\n",
        &[],
        &[],
    )
    .await
    {
        assert!(cause(&text).contains("an access past the end of the memory-mapped file"), "{text}");
        assert!(cause(&text).contains("journal.bin"), "{text}");
    }
}

#[tokio::test]
async fn a_core_from_somewhere_else_without_its_program_asks_for_it() {
    let mut notes = note(1, b"CORE\0", &prstatus(7, 11, 1, 1));
    notes.extend(note(3, b"CORE\0", &prpsinfo(7, "trader", "./trader")));
    notes.extend(note(0x4649_4c45, b"CORE\0", &nt_file(&[(0x5000, 0x6000, "/opt/nowhere/trader")])));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("core"), synthetic_core(&notes, &[(0x5000, 0x1000, 5)])).unwrap();
    let tool = CoreTriageTool { cwd: dir.path().to_path_buf() };
    let err = crate::test_support::call(&tool, serde_json::json!({"core": "core"})).await.unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("names the program `trader`") && text.contains("pass `program`") && text.contains("sysroot"), "{text}");
}

/// A core "from another machine" (written by gdb's gcore, its crashing
/// library moved under a sysroot): with `sysroot`, the crash site is the
/// library function, read from the library's own debug info.
#[tokio::test]
async fn a_foreign_core_is_triaged_through_its_sysroot() {
    let Some(fixture) = crate::corefile::foreign_core::build() else {
        return live_skip("needs cc and gdb");
    };
    let root = fixture.program.parent().unwrap().to_path_buf();
    let tool = CoreTriageTool { cwd: root.clone() };
    let text = crate::test_support::call(
        &tool,
        serde_json::json!({
            "core": fixture.core.to_string_lossy(),
            "program": fixture.program.to_string_lossy(),
            "sysroot": fixture.sysroot.to_string_lossy(),
        }),
    )
    .await
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(text.contains("CRASH: SIGSEGV at address 0x7f0000000004"), "{text}");
    assert!(text.contains("CRASH SITE (first frame in your code): queue_find() at q.c:3"), "{text}");
    assert!(cause(&text).contains("an invalid memory access at 0x7f0000000004 in queue_find()"), "{text}");
    assert!(!text.contains("NOT RUN"), "{text}");
}

#[test]
fn gdbs_siginfo_fills_only_what_the_notes_lack() {
    let answer = GdbAnswer {
        core: Some(GdbCore {
            siginfo: Some(GdbSiginfo { signo: 11, code: 1, addr: Some("0x18".into()) }),
            ..GdbCore::default()
        }),
        ..GdbAnswer::default()
    };
    let mut bare = CoreNotes { cursig: Some(11), ..CoreNotes::default() };
    fill_from_gdb(&mut bare, &answer);
    assert_eq!((bare.signo, bare.code, bare.addr), (Some(11), Some(1), Some(0x18)));
    let mut kernel = CoreNotes { signo: Some(11), code: Some(1), addr: Some(0x8), ..CoreNotes::default() };
    fill_from_gdb(&mut kernel, &answer);
    assert_eq!(kernel.addr, Some(0x8), "the core's own note wins");
}
