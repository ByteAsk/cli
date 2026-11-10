//! `debug_core`: triage a core dump in one call — the crash site, the faulting
//! thread and the likely cause — the way a senior engineer reads a production
//! core before deciding what to do next.
//!
//! Facts come from two independent places and are kept apart from inference:
//! the core's own ELF notes, read here (signal, fault address, the thread the
//! kernel dumped first, the memory map), and gdb running a fixed driver
//! ([`DRIVER`]) for stacks, the faulting instruction, registers, glibc's
//! saved abort message and an uncaught exception's type. The "likely cause"
//! is then derived from those facts by fixed rules, and says which ones.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;

use crate::run::RunOutcome;
use crate::run::run_capped_env;
use crate::run::timeout_from;
use crate::tools::runtime_util::ScratchDir;
use crate::tools::runtime_util::cap;
use crate::tools::runtime_util::display_path;
use crate::tools::runtime_util::not_run;
use crate::tools::runtime_util::signal_name;
use crate::tools::runtime_util::source_line;
use crate::tools::runtime_util::tail_lines;

const TOOL_NAME: &str = "debug_core";
const OUTPUT_CAP: usize = 9000;
const DRIVER: &str = include_str!("core_driver.py");

pub struct CoreTriageTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreArgs {
    core: String,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    sysroot: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

// ------------------------------ the core's notes ------------------------------

const ET_CORE: u16 = 4;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const NT_PRSTATUS: u32 = 1;
const NT_PRPSINFO: u32 = 3;
const NT_FILE: u32 = 0x4649_4c45;
const NT_SIGINFO: u32 = 0x5349_4749;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const MAX_NOTE_BYTES: u64 = 64 << 20;

/// One memory mapping of the crashed process (a PT_LOAD of the core).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub start: u64,
    pub end: u64,
    pub read: bool,
    pub write: bool,
    pub exec: bool,
    /// The file mapped there, from NT_FILE, if any.
    pub file: Option<String>,
}

impl Mapping {
    fn perms(&self) -> String {
        format!(
            "{}{}{}",
            if self.read { 'r' } else { '-' },
            if self.write { 'w' } else { '-' },
            if self.exec { 'x' } else { '-' }
        )
    }
}

/// What the kernel wrote into the core about the crash itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CoreNotes {
    pub machine: u16,
    /// Thread ids in note order; the kernel writes the thread that took the
    /// fatal signal FIRST.
    pub threads: Vec<i64>,
    /// From the first NT_PRSTATUS: the signal being handled.
    pub cursig: Option<i32>,
    pub signo: Option<i32>,
    pub code: Option<i32>,
    pub addr: Option<u64>,
    /// For a signal sent by a process (kill/tgkill): who sent it.
    pub sender_pid: Option<i64>,
    pub pid: Option<i64>,
    pub fname: Option<String>,
    pub args: Option<String>,
    pub pc: Option<u64>,
    pub sp: Option<u64>,
    pub maps: Vec<Mapping>,
}

pub(crate) fn read_core_notes(core: &Path) -> Result<CoreNotes, String> {
    let mut file = File::open(core).map_err(|e| format!("cannot open {}: {e}", core.display()))?;
    let mut h = [0u8; 64];
    file.read_exact(&mut h)
        .map_err(|_| format!("{} is not an ELF core file (too short)", core.display()))?;
    if &h[0..4] != b"\x7fELF" {
        return Err(format!("{} is not an ELF core file", core.display()));
    }
    if h[4] != 2 || h[5] != 1 {
        return Err(format!(
            "{} is not a 64-bit little-endian core, which is all this reads",
            core.display()
        ));
    }
    if u16_at(&h, 16) != ET_CORE {
        return Err(format!(
            "{} is an ELF file but not a core dump",
            core.display()
        ));
    }
    let machine = u16_at(&h, 18);
    let (phoff, phentsize, phnum) = (
        u64_at(&h, 32),
        u16_at(&h, 54) as usize,
        u16_at(&h, 56) as usize,
    );
    if phentsize < 56 {
        return Err("malformed ELF program header table".to_string());
    }
    let mut table = vec![0u8; phentsize * phnum];
    file.seek(SeekFrom::Start(phoff)).map_err(io)?;
    file.read_exact(&mut table).map_err(io)?;
    let mut notes = CoreNotes {
        machine,
        ..CoreNotes::default()
    };
    let mut files: Vec<(u64, u64, String)> = Vec::new();
    for ph in table.chunks_exact(phentsize) {
        let p_type = u32_at(ph, 0);
        let flags = u32_at(ph, 4);
        let offset = u64_at(ph, 8);
        let vaddr = u64_at(ph, 16);
        let filesz = u64_at(ph, 32);
        let memsz = u64_at(ph, 40);
        if p_type == PT_LOAD {
            notes.maps.push(Mapping {
                start: vaddr,
                end: vaddr.saturating_add(memsz),
                read: flags & 4 != 0,
                write: flags & 2 != 0,
                exec: flags & 1 != 0,
                file: None,
            });
        } else if p_type == PT_NOTE && filesz > 0 {
            if filesz > MAX_NOTE_BYTES {
                return Err("a note segment of the core is implausibly large".to_string());
            }
            let mut buf = vec![0u8; filesz as usize];
            file.seek(SeekFrom::Start(offset)).map_err(io)?;
            file.read_exact(&mut buf).map_err(io)?;
            parse_notes(&buf, &mut notes, &mut files);
        }
    }
    for map in &mut notes.maps {
        if let Some((_, _, path)) = files
            .iter()
            .find(|(s, e, _)| *s <= map.start && map.start < *e)
        {
            map.file = Some(path.clone());
        }
    }
    Ok(notes)
}

fn parse_notes(buf: &[u8], notes: &mut CoreNotes, files: &mut Vec<(u64, u64, String)>) {
    let mut at = 0usize;
    while at + 12 <= buf.len() {
        let namesz = u32_at(buf, at) as usize;
        let descsz = u32_at(buf, at + 4) as usize;
        let kind = u32_at(buf, at + 8);
        let desc_at = at + 12 + align4(namesz);
        let Some(desc) = buf.get(desc_at..desc_at.saturating_add(descsz)) else {
            break;
        };
        match kind {
            NT_PRSTATUS if desc.len() >= 112 => {
                let tid = i64::from(u32_at(desc, 32) as i32);
                if notes.threads.is_empty() {
                    notes.cursig = Some(i32::from(u16_at(desc, 12)));
                    let (pc, sp) = match notes.machine {
                        // user_regs_struct: rip is word 16, rsp word 19.
                        EM_X86_64 if desc.len() >= 112 + 27 * 8 => {
                            (u64_at(desc, 112 + 16 * 8), u64_at(desc, 112 + 19 * 8))
                        }
                        // user_pt_regs: x0..x30, sp, pc.
                        EM_AARCH64 if desc.len() >= 112 + 34 * 8 => {
                            (u64_at(desc, 112 + 32 * 8), u64_at(desc, 112 + 31 * 8))
                        }
                        _ => (0, 0),
                    };
                    if pc != 0 {
                        notes.pc = Some(pc);
                        notes.sp = Some(sp);
                    }
                }
                notes.threads.push(tid);
            }
            NT_PRPSINFO if desc.len() >= 136 => {
                notes.pid = Some(i64::from(u32_at(desc, 24) as i32));
                notes.fname = Some(c_str(&desc[40..56]));
                notes.args = Some(c_str(&desc[56..136]));
            }
            NT_SIGINFO if desc.len() >= 24 && notes.signo.is_none() => {
                let signo = u32_at(desc, 0) as i32;
                let code = u32_at(desc, 8) as i32;
                notes.signo = Some(signo);
                notes.code = Some(code);
                if matches!(signo, 4 | 7 | 8 | 11) && code > 0 {
                    notes.addr = Some(u64_at(desc, 16));
                } else if code <= 0 {
                    notes.sender_pid = Some(i64::from(u32_at(desc, 16) as i32));
                }
            }
            NT_FILE => files.extend(parse_nt_file(desc)),
            _ => {}
        }
        at = desc_at + align4(descsz);
    }
}

fn parse_nt_file(desc: &[u8]) -> Vec<(u64, u64, String)> {
    if desc.len() < 16 {
        return Vec::new();
    }
    let count = u64_at(desc, 0) as usize;
    let Some(names_at) = count
        .checked_mul(24)
        .and_then(|n| n.checked_add(16))
        .filter(|&n| n <= desc.len())
    else {
        return Vec::new();
    };
    let names = desc[names_at..].split(|&b| b == 0);
    (0..count)
        .zip(names)
        .map(|(i, name)| {
            let e = 16 + i * 24;
            (
                u64_at(desc, e),
                u64_at(desc, e + 8),
                String::from_utf8_lossy(name).into_owned(),
            )
        })
        .collect()
}

fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or([0; 4]))
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]))
}
fn io(err: std::io::Error) -> String {
    err.to_string()
}

/// The executable the core came from: the file mapped at the lowest address
/// (the kernel maps the program before its interpreter and libraries) whose
/// name agrees with the process name the core records.
fn program_from_core(notes: &CoreNotes, sysroot: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<&Mapping> = notes.maps.iter().filter(|m| m.file.is_some()).collect();
    candidates.sort_by_key(|m| m.start);
    let fname = notes.fname.as_deref().unwrap_or("");
    candidates.into_iter().find_map(|m| {
        let path = m.file.as_deref()?;
        let base = path.rsplit('/').next().unwrap_or(path);
        // The kernel truncates the process name to 15 bytes.
        if !fname.is_empty() && !base.starts_with(fname) {
            return None;
        }
        let local = match sysroot {
            Some(root) => root.join(path.trim_start_matches('/')),
            None => PathBuf::from(path),
        };
        local.is_file().then_some(local)
    })
}

// ------------------------------ gdb's answer ------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct Frame {
    #[serde(default)]
    pub level: u32,
    #[serde(default)]
    pub function: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub library: Option<String>,
    #[serde(default)]
    pub pc: Option<String>,
    #[serde(default)]
    pub user: bool,
    #[serde(default)]
    pub args: Vec<(String, String)>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct Exception {
    #[serde(rename = "type", default)]
    pub ty: Option<String>,
    #[serde(default)]
    pub what: Option<String>,
    #[serde(default)]
    pub frame: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct GdbSiginfo {
    pub signo: i32,
    pub code: i32,
    #[serde(default)]
    pub addr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct GdbCore {
    #[serde(default)]
    pub siginfo: Option<GdbSiginfo>,
    #[serde(default)]
    pub instruction: Option<String>,
    #[serde(default)]
    pub registers: BTreeMap<String, String>,
    #[serde(default)]
    pub abort_msg: Option<String>,
    #[serde(default)]
    pub exception: Option<Exception>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct GdbThread {
    pub tid: i64,
    #[serde(default)]
    pub faulting: bool,
    #[serde(default)]
    pub frames: Vec<Frame>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Default)]
pub(crate) struct GdbAnswer {
    pub core: Option<GdbCore>,
    pub threads: Vec<GdbThread>,
    pub done: bool,
    pub error: Option<String>,
}

fn read_answer(path: &Path) -> GdbAnswer {
    let mut answer = GdbAnswer::default();
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("kind").and_then(|k| k.as_str()) {
            Some("core") => answer.core = serde_json::from_value(value).ok(),
            Some("thread") => {
                if let Ok(t) = serde_json::from_value(value) {
                    answer.threads.push(t);
                }
            }
            Some("done") => answer.done = true,
            Some("error") => {
                answer.error = value
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    answer
}

// ------------------------------ the likely cause ------------------------------

/// Everything the rules read, from both sources.
pub(crate) struct Facts<'a> {
    pub notes: &'a CoreNotes,
    pub gdb: Option<&'a GdbCore>,
    pub faulting: Option<&'a GdbThread>,
    pub others: Vec<&'a GdbThread>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cause {
    /// One line: what went wrong.
    pub title: String,
    /// The facts the conclusion rests on.
    pub because: Vec<String>,
    /// The concrete next step.
    pub next: String,
}

fn has_frame(frames: &[Frame], names: &[&str]) -> Option<usize> {
    frames.iter().position(|f| {
        f.function.as_deref().is_some_and(|n| {
            names
                .iter()
                .any(|want| n == *want || n.starts_with(&format!("{want}(")))
        })
    })
}

/// The first frame in the user's own code, at or after `from`.
fn user_from(frames: &[Frame], from: usize) -> Option<&Frame> {
    frames.iter().skip(from).find(|f| f.user)
}

fn site(frame: Option<&Frame>, cwd: &Path) -> String {
    match frame {
        Some(f) => frame_text(f, cwd),
        None => "code with no debug info".to_string(),
    }
}

fn frame_text(f: &Frame, cwd: &Path) -> String {
    let name = f
        .function
        .clone()
        .unwrap_or_else(|| f.pc.clone().unwrap_or_else(|| "??".into()));
    match (&f.file, f.line) {
        (Some(file), Some(line)) => format!("{name}() at {}:{line}", display_path(file, cwd)),
        _ => match &f.library {
            Some(lib) => format!("{name} in {}", lib.rsplit('/').next().unwrap_or(lib)),
            None => name,
        },
    }
}

/// The base register of the faulting instruction's memory operand, AT&T
/// (`0x8(%rax)`, `(%rax,%rcx,8)`) or AArch64 (`[x1, #8]`) syntax.
pub(crate) fn memory_base(insn: &str) -> Option<(String, i64)> {
    if let Some(open) = insn.find("(%") {
        let reg: String = insn[open + 2..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let disp_text = insn[..open]
            .rsplit([' ', ',', '\t'])
            .next()
            .unwrap_or("")
            .trim();
        let disp = parse_int(disp_text).unwrap_or(0);
        return (!reg.is_empty()).then_some((reg, disp));
    }
    if let Some(open) = insn.find('[') {
        let inner = &insn[open + 1..insn[open..].find(']').map(|c| open + c)?];
        let mut parts = inner.split(',').map(str::trim);
        let reg = parts.next()?.to_string();
        let disp = parts
            .next()
            .and_then(|d| parse_int(d.trim_start_matches('#')))
            .unwrap_or(0);
        return Some((reg, disp));
    }
    None
}

fn parse_int(text: &str) -> Option<i64> {
    let (neg, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let value = match body.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None if body.is_empty() => 0,
        None => body.parse().ok()?,
    };
    Some(if neg { -value } else { value })
}

fn reg_value(gdb: Option<&GdbCore>, reg: &str) -> Option<u64> {
    let text = gdb?.registers.get(reg)?;
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// A pointer whose bytes are mostly printable text ("AAAAAAAA" is
/// 0x4141414141414141): characters were copied over it. Such a value is
/// usually not canonical either, which x86-64 reports as SIGSEGV with
/// si_code SI_KERNEL and address 0, so the register is the only evidence.
fn printable_ascii(value: u64) -> Option<String> {
    let bytes = value.to_le_bytes();
    let text: String = bytes.iter().map(|&b| b as char).collect();
    (bytes
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ')
        .count()
        >= 6)
        .then_some(text)
}

const HEAP_INTERNALS: &[&str] = &[
    "_int_malloc",
    "_int_free",
    "_int_realloc",
    "malloc_consolidate",
    "unlink_chunk",
    "__libc_malloc",
    "__GI___libc_malloc",
    "__libc_free",
    "__GI___libc_free",
    "__libc_realloc",
    "tcache_get",
    "tcache_put",
    "malloc",
    "free",
    "cfree",
    "realloc",
];

pub(crate) fn likely_cause(f: &Facts, cwd: &Path) -> Cause {
    let signo = f.notes.signo.or(f.notes.cursig).unwrap_or(0);
    let code = f.notes.code.unwrap_or(0);
    let addr = f.notes.addr;
    let frames: &[Frame] = f.faulting.map(|t| t.frames.as_slice()).unwrap_or(&[]);
    let user = user_from(frames, 0);
    let gdb = f.gdb;
    let insn = gdb.and_then(|g| g.instruction.as_deref());
    let abort_msg = gdb.and_then(|g| g.abort_msg.as_deref());
    let mut because = Vec::new();
    let asan = "rebuild with AddressSanitizer and run the same input (cpp_sanitize): it stops at the \
                FIRST bad access, which is usually long before this crash";
    let reverse = |what: &str| {
        format!(
            "find who wrote {what}: record the failing run with debug_record and ask debug_reverse \
             who_wrote; or {asan}"
        )
    };

    match signo {
        6 => {
            if let Some(exc) = gdb
                .and_then(|g| g.exception.as_ref())
                .filter(|e| e.ty.is_some())
            {
                let ty = exc.ty.as_deref().unwrap_or("?");
                let thrower = user_from(frames, exc.frame as usize);
                because.push(format!(
                    "std::terminate ran from __cxa_throw (frame #{})",
                    exc.frame
                ));
                if let Some(what) = &exc.what {
                    because.push(format!(
                        "what(): \"{what}\" (read from the exception object in the core)"
                    ));
                }
                let on_thread = if f.faulting.map(|t| t.tid) != f.notes.pid {
                    " on a std::thread — an exception escaping a thread's function terminates the whole process"
                } else {
                    ""
                };
                return Cause {
                    title: format!("an uncaught C++ exception {ty} thrown from {}{on_thread}", site(thrower, cwd)),
                    because,
                    next: "catch it where it can be handled (or at the thread's entry point), or fix what \
                           made it throw"
                        .to_string(),
                };
            }
            if let Some(i) = has_frame(frames, &["std::thread::~thread"]) {
                return Cause {
                    title: format!(
                        "a std::thread was destroyed while still joinable (never joined or detached) in {}",
                        site(user_from(frames, i), cwd)
                    ),
                    because: vec!["std::terminate was called from std::thread::~thread".to_string()],
                    next: "join() or detach() every std::thread before it goes out of scope (or use std::jthread)"
                        .to_string(),
                };
            }
            if let Some(i) = has_frame(frames, &["__cxa_pure_virtual"]) {
                return Cause {
                    title: format!("a pure virtual function was called from {}", site(user_from(frames, i), cwd)),
                    because: vec!["__cxa_pure_virtual is on the stack".to_string()],
                    next: "the object was used during its constructor/destructor or after it was deleted; \
                           check the object's lifetime"
                        .to_string(),
                };
            }
            if let Some(msg) = abort_msg {
                because.push(format!("glibc's saved abort message: \"{msg}\""));
                if msg.contains("Assertion") {
                    return Cause {
                        title: format!("an assertion failed in {}", site(user, cwd)),
                        because,
                        next: "the asserted condition was false: look at the values that made it so (the \
                               arguments in the backtrace below)"
                            .to_string(),
                    };
                }
                if msg.contains("stack smashing detected") {
                    return Cause {
                        title: format!(
                            "a stack buffer overflow in {}: a local array was written past its end, and \
                             the stack protector caught it as the function returned",
                            site(user, cwd)
                        ),
                        because,
                        next: format!(
                            "look for a copy into a fixed-size local buffer in that function; {asan}"
                        ),
                    };
                }
                if msg.contains("buffer overflow detected") {
                    return Cause {
                        title: format!(
                            "_FORTIFY_SOURCE caught a buffer overflow in {}",
                            site(user, cwd)
                        ),
                        because,
                        next: format!(
                            "a checked copy (memcpy/strcpy/sprintf…) was given a size larger than its \
                                       destination; {asan}"
                        ),
                    };
                }
                let heap = [
                    "double free",
                    "free(): ",
                    "malloc(): ",
                    "corrupted",
                    "realloc(): ",
                    "munmap_chunk",
                    "malloc_consolidate",
                ];
                if heap.iter().any(|h| msg.contains(h)) {
                    return Cause {
                        title: format!(
                            "heap corruption, detected by glibc in {}: the place it was DETECTED, not \
                             necessarily the place it happened",
                            site(user, cwd)
                        ),
                        because,
                        next: format!(
                            "a double free, use after free or out-of-bounds write corrupted the allocator's \
                             data earlier; {asan}"
                        ),
                    };
                }
            }
            if has_frame(frames, &["std::terminate"]).is_some() {
                return Cause {
                    title: format!("std::terminate was called from {}", site(user, cwd)),
                    because: vec!["std::terminate is on the stack with no exception being thrown".to_string()],
                    next: "typical causes: an exception left a noexcept function or a destructor, or one was \
                           thrown while another was unwinding"
                        .to_string(),
                };
            }
            if let (Some(sender), Some(pid)) = (f.notes.sender_pid, f.notes.pid)
                && sender != pid
                && code == 0
            {
                return Cause {
                    title: format!("killed with SIGABRT by another process (pid {sender})"),
                    because: vec![format!("siginfo: sent with kill() by pid {sender}, not by this process")],
                    next: "find who sent it (a watchdog, a supervisor, a person); the program itself did not abort"
                        .to_string(),
                };
            }
            Cause {
                title: format!("abort() was called from {}", site(user, cwd)),
                because,
                next: "read the code at the crash site: something decided to stop the program"
                    .to_string(),
            }
        }
        11 => {
            let base = insn.and_then(memory_base);
            let base_value = base.as_ref().and_then(|(reg, _)| reg_value(gdb, reg));
            if let Some(insn) = insn {
                let regs = base
                    .as_ref()
                    .and_then(|(reg, _)| base_value.map(|v| format!(" with {reg} = {v:#x}")))
                    .unwrap_or_default();
                because.push(format!("faulting instruction: {insn}{regs}"));
            }
            let pc = f
                .notes
                .pc
                .or_else(|| reg_value(gdb, "rip"))
                .or_else(|| reg_value(gdb, "pc"));
            let sp = f
                .notes
                .sp
                .or_else(|| reg_value(gdb, "rsp"))
                .or_else(|| reg_value(gdb, "sp"));
            if code == 128 {
                because.push(
                    "si_code SI_KERNEL: a general-protection fault, i.e. an address that is not \
                              canonical — no valid pointer looks like that"
                        .to_string(),
                );
                if let Some(v) = base_value {
                    if let Some(text) = printable_ascii(v) {
                        return Cause {
                            title: format!(
                                "a pointer was overwritten with text: {v:#x} reads as \"{text}\" — a buffer \
                                 overflow wrote characters over it (in {})",
                                site(user, cwd)
                            ),
                            because,
                            next: format!(
                                "find the copy that overran the buffer next to that pointer; {asan}"
                            ),
                        };
                    }
                    return Cause {
                        title: format!(
                            "a wild pointer {v:#x} (not a possible address) was dereferenced in {}",
                            site(user, cwd)
                        ),
                        because,
                        next: reverse("that pointer"),
                    };
                }
                return Cause {
                    title: format!(
                        "a wild (non-canonical) pointer was dereferenced in {}",
                        site(user, cwd)
                    ),
                    because,
                    next: reverse("that pointer"),
                };
            }
            let addr = addr.unwrap_or(0);
            because.push(format!(
                "fault address {addr:#x} ({})",
                match code {
                    1 => "SEGV_MAPERR: nothing is mapped there",
                    2 => "SEGV_ACCERR: mapped, but not for this kind of access",
                    3 => "SEGV_BNDERR",
                    4 => "SEGV_PKUERR: a memory protection key refused it",
                    _ => "si_code unknown",
                }
            ));
            let executable = |a: u64| {
                f.notes
                    .maps
                    .iter()
                    .any(|m| m.start <= a && a < m.end && m.exec)
            };
            if pc == Some(addr) && !executable(addr) {
                let caller = frames.iter().skip(1).find(|fr| fr.user);
                return Cause {
                    title: format!(
                        "a call or jump to the bad address {addr:#x}: a corrupted function pointer, vtable \
                         pointer or return address{}",
                        caller
                            .map(|c| format!(" — the call was made from {}", frame_text(c, cwd)))
                            .unwrap_or_default()
                    ),
                    because,
                    next: reverse(
                        "the function pointer (or check the object holding the vtable is still alive)",
                    ),
                };
            }
            let repeated = frames
                .first()
                .and_then(|top| top.function.as_deref())
                .map(|name| {
                    frames
                        .iter()
                        .filter(|fr| fr.function.as_deref() == Some(name))
                        .count()
                });
            let near_stack = sp.is_some_and(|sp| addr.abs_diff(sp) <= 64 * 1024);
            if near_stack
                || (repeated.unwrap_or(0) >= 64 && f.faulting.is_some_and(|t| t.truncated))
            {
                if let Some(n) = repeated.filter(|&n| n >= 16) {
                    because.push(format!(
                        "{} appears {n}{} times in a row on the stack",
                        frames[0].function.as_deref().unwrap_or("?"),
                        if f.faulting.is_some_and(|t| t.truncated) {
                            "+"
                        } else {
                            ""
                        }
                    ));
                }
                if let Some(sp) = sp {
                    because.push(format!(
                        "the fault is {} bytes from the stack pointer {sp:#x}",
                        addr.abs_diff(sp)
                    ));
                }
                return Cause {
                    title: match repeated.filter(|&n| n >= 16) {
                        Some(_) => format!(
                            "a stack overflow: {} recursed until the stack ran out",
                            frames[0].function.as_deref().unwrap_or("a function")
                        ),
                        None => format!("a stack overflow in {}: the thread ran out of stack", site(user, cwd)),
                    },
                    because,
                    next: "find the missing recursion base case, or move large local arrays off the stack (or \
                           raise the thread's stack size)"
                        .to_string(),
                };
            }
            if has_frame(frames, HEAP_INTERNALS).is_some() {
                return Cause {
                    title: format!(
                        "heap corruption: the crash is inside the allocator, called from {} — that call is a \
                         victim; the damage was done earlier",
                        site(user, cwd)
                    ),
                    because,
                    next: format!("a double free, use after free or out-of-bounds write; {asan}"),
                };
            }
            if code == 2
                && let Some(map) = f
                    .notes
                    .maps
                    .iter()
                    .find(|m| m.start <= addr && addr < m.end)
            {
                let what = map
                    .file
                    .clone()
                    .unwrap_or_else(|| "anonymous memory".to_string());
                because.push(format!(
                    "{addr:#x} is in a {} mapping of {what}",
                    map.perms()
                ));
                if !map.write && map.read {
                    return Cause {
                        title: format!(
                            "a write to read-only memory in {}: a string literal, const data or code was \
                             modified",
                            site(user, cwd)
                        ),
                        because,
                        next: "copy the data into writable storage before changing it (a string literal is \
                               const char[], never char*)"
                            .to_string(),
                    };
                }
                if !map.read {
                    return Cause {
                        title: format!(
                            "an access to a guard (no-access) page in {}: a run past the end of a buffer or a \
                             stack, or a freed-and-protected region",
                            site(user, cwd)
                        ),
                        because,
                        next: asan.to_string(),
                    };
                }
            }
            // NULL is decided by the POINTER, when the instruction says which
            // register held it: fault address 0x10 from a pointer holding 0x10
            // is a bad pointer, not a NULL one.
            let null_pointer = match base_value {
                Some(v) => v == 0,
                None => addr < 4096,
            };
            if null_pointer {
                let offset = base.as_ref().map(|(_, d)| *d).unwrap_or(addr as i64);
                return Cause {
                    title: format!(
                        "a NULL pointer dereference in {}{}",
                        site(user, cwd),
                        if offset > 0 {
                            format!(", reading the member at offset {offset:#x} of a NULL object")
                        } else {
                            String::new()
                        }
                    ),
                    because,
                    next: reverse(
                        "the pointer (it should never have been NULL there), or add the missing NULL check",
                    ),
                };
            }
            if base_value.map_or(addr < 65536, |v| v < 65536) {
                let shown = base_value.map(|v| format!(" {v:#x}")).unwrap_or_default();
                return Cause {
                    title: format!(
                        "a bad pointer{shown} — not NULL, but too small to be a real address — was \
                         dereferenced in {}: a small integer or a garbage value was used as a pointer",
                        site(user, cwd)
                    ),
                    because,
                    next: reverse("that pointer"),
                };
            }
            Cause {
                title: format!(
                    "an invalid memory access at {addr:#x} in {}: a dangling pointer (used after free or \
                     unmap), an out-of-bounds index, or an uninitialized pointer",
                    site(user, cwd)
                ),
                because,
                next: reverse("that pointer"),
            }
        }
        8 => {
            because.push(format!("si_code {code}"));
            let zero_arg = user.and_then(|u| {
                u.args
                    .iter()
                    .find(|(_, v)| v == "0")
                    .map(|(n, _)| n.clone())
            });
            if let Some(name) = &zero_arg {
                because.push(format!("argument `{name}` is 0"));
            }
            let title = match code {
                1 => format!(
                    "an integer division by zero (or INT_MIN / -1) in {}",
                    site(user, cwd)
                ),
                2 => format!("an integer overflow trap in {}", site(user, cwd)),
                3..=8 => format!(
                    "a floating-point exception trap in {} (traps were enabled with feenableexcept)",
                    site(user, cwd)
                ),
                _ => format!("an arithmetic exception in {}", site(user, cwd)),
            };
            Cause {
                title,
                because,
                next: "guard the divisor (a zero count, lot size or interval) before dividing"
                    .to_string(),
            }
        }
        4 => {
            let insn = insn.unwrap_or("");
            because.push(format!(
                "faulting instruction: {}",
                if insn.is_empty() { "unknown" } else { insn }
            ));
            let mnemonic = insn.split_whitespace().next().unwrap_or("");
            if matches!(mnemonic, "ud2" | "udf" | "brk") {
                return Cause {
                    title: format!(
                        "a trap instruction ({mnemonic}) was reached in {}: __builtin_trap(), \
                         __builtin_unreachable(), or a compiler-inserted trap for code it considered impossible",
                        site(user, cwd)
                    ),
                    because,
                    next: "typical cause: undefined behaviour the optimizer exploited (falling off the end of a \
                           non-void function, an unhandled enum value), or a -fsanitize=…-trap check; build with \
                           -Wreturn-type -Werror and UBSan (cpp_sanitize) to see which"
                        .to_string(),
                };
            }
            let vector = ["zmm", "ymm", "%k", "{k"].iter().any(|r| insn.contains(r))
                || [
                    "vp", "kmov", "kortest", "andn", "pdep", "pext", "shlx", "sarx", "tzcnt",
                    "lzcnt", "movbe", "adcx", "adox", "vfmadd",
                ]
                .iter()
                .any(|p| mnemonic.starts_with(p));
            if vector {
                return Cause {
                    title: format!(
                        "this CPU does not support the instruction `{mnemonic}` in {}: the binary was built for a \
                         newer CPU (-march=native, -mavx2, -mavx512f…) than the one it ran on",
                        site(user, cwd)
                    ),
                    because,
                    next: "build for the oldest CPU in the fleet (an explicit -march), or dispatch on cpuid at run \
                           time"
                        .to_string(),
                };
            }
            Cause {
                title: format!("an illegal instruction in {}", site(user, cwd)),
                because,
                next: "execution reached bytes that are not valid code: a corrupted function pointer or return \
                       address, or a binary built for another CPU"
                    .to_string(),
            }
        }
        7 => {
            let addr = addr.unwrap_or(0);
            because.push(format!("fault address {addr:#x}, si_code {code}"));
            let file = f
                .notes
                .maps
                .iter()
                .find(|m| m.start <= addr && addr < m.end)
                .and_then(|m| m.file.clone());
            match (code, file) {
                (2, Some(file)) => Cause {
                    title: format!(
                        "an access past the end of the memory-mapped file {file} in {}: the file is shorter than \
                         the mapping (it was truncated while mapped)",
                        site(user, cwd)
                    ),
                    because,
                    next: "never truncate a file another process maps; size the mapping from the file's current \
                           length, or use a sealed / preallocated file"
                        .to_string(),
                },
                (2, None) => Cause {
                    title: format!(
                        "a bus error at {addr:#x} in {}: a mapping with no backing page (a truncated file, or an \
                         exhausted huge-page pool)",
                        site(user, cwd)
                    ),
                    because,
                    next: "check the size of whatever is mapped there and the huge-page pool \
                           (/proc/meminfo HugePages_Free)"
                        .to_string(),
                },
                (1, _) => Cause {
                    title: format!("a misaligned memory access in {}", site(user, cwd)),
                    because,
                    next: "align the data, or copy it with memcpy instead of casting the pointer".to_string(),
                },
                _ => Cause {
                    title: format!("a bus error in {}", site(user, cwd)),
                    because,
                    next: "check what is mapped at the fault address".to_string(),
                },
            }
        }
        0 => Cause {
            title: "the core records no signal".to_string(),
            because,
            next: "it may have been written by gcore from a live process, not by a crash"
                .to_string(),
        },
        other => {
            let name = signal_name(other).unwrap_or("an unknown signal");
            if let Some(sender) = f.notes.sender_pid.filter(|_| code <= 0) {
                because.push(format!("sent by pid {sender}"));
            }
            Cause {
                title: format!("the process was stopped by {name} in {}", site(user, cwd)),
                because,
                next: "find who sent the signal and why".to_string(),
            }
        }
    }
}

/// Other threads that were in the same function of the user's code as the
/// faulting thread: shared state there is a race candidate.
fn same_function_threads<'a>(
    faulting: Option<&GdbThread>,
    others: &[&'a GdbThread],
) -> Vec<&'a GdbThread> {
    let Some(name) = faulting
        .and_then(|t| user_from(&t.frames, 0))
        .and_then(|f| f.function.as_deref())
    else {
        return Vec::new();
    };
    others
        .iter()
        .copied()
        .filter(|t| {
            t.frames
                .iter()
                .any(|f| f.user && f.function.as_deref() == Some(name))
        })
        .collect()
}

impl<'call> ToolExecutor<ToolCall<'call>> for CoreTriageTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "core".to_string(),
            JsonSchema::string(Some(
                "Path to the core dump (relative to the workspace or absolute).".to_string(),
            )),
        );
        properties.insert(
            "program".to_string(),
            JsonSchema::string(Some(
                "The executable that crashed. Default: the one the core itself names, when that file exists \
                 here (or under `sysroot`)."
                    .to_string(),
            )),
        );
        properties.insert(
            "sysroot".to_string(),
            JsonSchema::string(Some(
                "For a core from ANOTHER machine: a directory holding copies of that machine's files at their \
                 original paths (sysroot/usr/lib/…, sysroot/opt/app/…), so its libraries resolve."
                    .to_string(),
            )),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::number(Some("Wall-clock limit for gdb (default 300).".to_string())),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Triage a core dump in ONE call: the signal and fault address, the faulting thread \
                (the one the kernel dumped first) and its crash site in your code, the faulting instruction \
                and the register it used, then the LIKELY CAUSE with the facts it rests on — NULL or wild \
                pointer (including one overwritten with text), stack overflow / runaway recursion, heap \
                corruption (glibc's saved abort message: double free, corrupted size…), a failed assert \
                with its message, an uncaught C++ exception with its type and what() text, a joinable \
                std::thread destroyed, division by zero, __builtin_trap/unreachable reached, an \
                instruction this CPU lacks (built with the wrong -march), a truncated memory-mapped file \
                (SIGBUS) — plus every thread's position and which other threads were in the same function \
                (a race hint). Start here for any core file or 'it crashed in production'; then dig deeper \
                with debug (custom gdb commands on the same core) or, to see what led up to it, \
                debug_record + debug_reverse. Read-only: it never runs the program. Linux ELF cores, gdb. \
                C/C++."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["core".to_string()]),
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

impl CoreTriageTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: CoreArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let core = resolve(&self.cwd, &args.core);
        let sysroot = args.sysroot.as_deref().map(|s| resolve(&self.cwd, s));
        if let Some(root) = &sysroot
            && !root.is_dir()
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "sysroot {} is not a directory",
                root.display()
            )));
        }
        let notes = read_core_notes(&core).map_err(FunctionCallError::RespondToModel)?;
        let program = match args.program.as_deref() {
            Some(p) => {
                let p = resolve(&self.cwd, p);
                if !p.is_file() {
                    return Err(FunctionCallError::RespondToModel(format!("program {} does not exist", p.display())));
                }
                p
            }
            None => program_from_core(&notes, sysroot.as_deref()).ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "the core names the program `{}`, but that file is not here{}: pass `program` (the exact \
                     binary that crashed){}",
                    notes.fname.as_deref().unwrap_or("?"),
                    sysroot.as_ref().map(|r| format!(" or under {}", r.display())).unwrap_or_default(),
                    if sysroot.is_none() { ", and `sysroot` if the core is from another machine" } else { "" }
                ))
            })?,
        };

        let timeout = timeout_from(args.timeout_sec);
        let scratch = ScratchDir::new("core-triage").map_err(|e| {
            FunctionCallError::RespondToModel(format!("cannot create a scratch directory: {e}"))
        })?;
        let driver = scratch.path().join("byteask_core_driver.py");
        let out = scratch.path().join("answer.jsonl");
        std::fs::write(&driver, DRIVER).map_err(|e| {
            FunctionCallError::RespondToModel(format!("cannot write the gdb driver: {e}"))
        })?;
        let root = std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        let query = serde_json::json!({ "user_roots": [root.to_string_lossy()] }).to_string();
        let argv = build_gdb_argv(&program, &core, sysroot.as_deref(), &driver);
        let envs = vec![
            ("BYTEASK_CORE_QUERY".to_string(), query),
            (
                "BYTEASK_CORE_OUT".to_string(),
                out.to_string_lossy().into_owned(),
            ),
        ];
        let (stderr, timed_out) =
            match run_capped_env("gdb", &argv, &self.cwd, timeout, &envs).await {
                Ok(RunOutcome::Finished { stdout, stderr, .. }) => {
                    (format!("{stdout}\n{stderr}"), None)
                }
                Ok(RunOutcome::TimedOut { secs }) => (String::new(), Some(secs)),
                // gdb missing: the notes alone still say a lot.
                Err(err) => (format!("{err:?}"), None),
            };
        let answer = read_answer(&out);
        let mut notes = notes;
        fill_from_gdb(&mut notes, &answer);
        let body = render(
            &notes, &core, &program, &answer, &stderr, timed_out, &self.cwd,
        );
        let cut = body.len() > OUTPUT_CAP;
        let result =
            crate::tools::runtime_util::verdict(TOOL_NAME, &body, cut, "Triaged the core file.");
        Ok(crate::report::report(
            TOOL_NAME,
            &args.core,
            cap(body, OUTPUT_CAP),
            result,
        ))
    }
}

/// A core written by gdb's `gcore` (or an older kernel) may carry no
/// NT_SIGINFO; gdb's `$_siginfo` then fills what the notes lack. The notes win
/// wherever they have the field.
fn fill_from_gdb(notes: &mut CoreNotes, answer: &GdbAnswer) {
    let Some(si) = answer.core.as_ref().and_then(|c| c.siginfo.as_ref()) else {
        return;
    };
    if notes.signo.is_none() && si.signo > 0 {
        notes.signo = Some(si.signo);
        notes.code = Some(si.code);
    }
    let fault = matches!(notes.signo, Some(4 | 7 | 8 | 11)) && notes.code.unwrap_or(0) > 0;
    if notes.addr.is_none() && fault {
        notes.addr = si
            .addr
            .as_deref()
            .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok());
    }
}

fn build_gdb_argv(
    program: &Path,
    core: &Path,
    sysroot: Option<&Path>,
    driver: &Path,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-q",
        "-nx",
        "-batch",
        "-iex",
        "set auto-load off",
        // No silent download of debug info for the user's binaries: that
        // would send their build ids to a third-party server.
        "-iex",
        "set debuginfod enabled off",
    ]
    .map(str::to_string)
    .to_vec();
    if let Some(root) = sysroot {
        argv.push("-iex".to_string());
        argv.push(format!("set sysroot {}", root.display()));
        let debug = root.join("usr/lib/debug");
        if debug.is_dir() {
            argv.push("-iex".to_string());
            argv.push(format!("set debug-file-directory {}", debug.display()));
        }
    }
    argv.extend(["-ex".to_string(), "set may-call-functions off".to_string()]);
    argv.extend(["-ex".to_string(), format!("source {}", driver.display())]);
    argv.push(program.to_string_lossy().into_owned());
    argv.push(core.to_string_lossy().into_owned());
    argv
}

fn render(
    notes: &CoreNotes,
    core: &Path,
    program: &Path,
    answer: &GdbAnswer,
    stderr: &str,
    timed_out: Option<u64>,
    cwd: &Path,
) -> String {
    let mut text = String::new();
    let command = notes
        .args
        .clone()
        .filter(|a| !a.is_empty())
        .or_else(|| notes.fname.clone())
        .unwrap_or_default();
    text.push_str(&format!(
        "core: {} — `{command}`{}, {} thread(s)\nprogram: {}\n",
        display_path(&core.to_string_lossy(), cwd),
        notes.pid.map(|p| format!(" (pid {p})")).unwrap_or_default(),
        notes.threads.len(),
        display_path(&program.to_string_lossy(), cwd)
    ));
    if stderr.contains("core file may not match specified executable") {
        text.push_str(
            "WARNING: gdb says this core may not come from this program build: the stack and lines below \
             can be wrong. Pass the exact binary that crashed as `program`.\n",
        );
    }

    let signo = notes.signo.or(notes.cursig);
    let signal = signo
        .map(|s| {
            signal_name(s)
                .map(str::to_string)
                .unwrap_or_else(|| format!("signal {s}"))
        })
        .unwrap_or_else(|| "no signal".to_string());
    let faulting = answer.threads.iter().find(|t| t.faulting);
    let tid = faulting.map(|t| t.tid).or(notes.threads.first().copied());
    let frames: &[Frame] = faulting.map(|t| t.frames.as_slice()).unwrap_or(&[]);
    let user = user_from(frames, 0);

    text.push_str(&format!(
        "\nCRASH: {signal}{}\n",
        notes
            .addr
            .filter(|_| matches!(signo, Some(4 | 7 | 8 | 11)))
            .map(|a| format!(" at address {a:#x}"))
            .unwrap_or_default()
    ));
    if let (Some(tid), Some(first)) = (tid, notes.threads.first()) {
        let index = notes
            .threads
            .iter()
            .position(|t| *t == tid)
            .map(|i| i + 1)
            .unwrap_or(1);
        text.push_str(&format!(
            "FAULTING THREAD: {tid} (thread {index} of {}{})\n",
            notes.threads.len(),
            if *first == tid {
                "; the kernel dumped it first"
            } else {
                ""
            }
        ));
    }
    if let Some(user) = user {
        text.push_str(&format!(
            "CRASH SITE (first frame in your code): {}\n",
            frame_text(user, cwd)
        ));
        if !user.args.is_empty() {
            let args = user
                .args
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("  args: {args}\n"));
        }
        if let Some(src) = user
            .file
            .as_deref()
            .zip(user.line)
            .and_then(|(f, l)| source_line(f, l))
        {
            text.push_str(&format!("  | {src}\n"));
        }
    }

    let gdb_ran = answer.core.is_some();
    let others: Vec<&GdbThread> = answer.threads.iter().filter(|t| !t.faulting).collect();
    let facts = Facts {
        notes,
        gdb: answer.core.as_ref(),
        faulting,
        others: others.clone(),
    };
    let cause = likely_cause(&facts, cwd);
    text.push_str(&format!("\nLIKELY CAUSE: {}\n", cause.title));
    for b in &cause.because {
        text.push_str(&format!("  because: {b}\n"));
    }
    let racing = same_function_threads(faulting, &facts.others);
    if !racing.is_empty() {
        text.push_str(&format!(
            "  also: {} other thread(s) ({}) were inside {} at the crash — if they share state there, a \
             data race is possible (cpp_sanitize sanitizer=\"tsan\")\n",
            racing.len(),
            racing.iter().map(|t| t.tid.to_string()).collect::<Vec<_>>().join(", "),
            user.and_then(|u| u.function.as_deref()).unwrap_or("that function")
        ));
    }
    text.push_str(&format!("NEXT: {}\n", cause.next));
    if !gdb_ran {
        text.push_str(
            "  (the cause above uses only the core's own notes: gdb did not answer, so it lacks the \
             instruction, stack and messages; treat it as a first guess)\n",
        );
    }

    if gdb_ran {
        text.push_str("\nbacktrace of the faulting thread (your code marked *):\n");
        let mut hidden = 0;
        let mut shown = 0;
        for f in frames {
            if f.user || f.level < 2 {
                if shown < 24 {
                    text.push_str(&format!(
                        "  {}#{} {}\n",
                        if f.user { "*" } else { " " },
                        f.level,
                        frame_text(f, cwd)
                    ));
                }
                shown += 1;
            } else {
                hidden += 1;
            }
        }
        if shown > 24 {
            text.push_str(&format!("  … {} more frame(s) of your code\n", shown - 24));
        }
        if hidden > 0 {
            text.push_str(&format!("  ({hidden} system frame(s) not shown)\n"));
        }
        if faulting.is_some_and(|t| t.truncated) {
            text.push_str("  (the stack is deeper than the frames read)\n");
        }
        if !others.is_empty() {
            text.push_str(&format!("\nother threads ({}):\n", others.len()));
            for t in &others {
                let place = user_from(&t.frames, 0)
                    .map(|f| frame_text(f, cwd))
                    .or_else(|| t.frames.first().map(|f| frame_text(f, cwd)))
                    .unwrap_or_else(|| "?".into());
                text.push_str(&format!("  {} — {place}\n", t.tid));
            }
        }
        if !answer.done {
            text.push_str("\nINCOMPLETE: gdb stopped before reading every thread; the lists above are partial.\n");
        }
    } else {
        let reason = if let Some(secs) = timed_out {
            not_run(
                &format!("gdb did not finish reading the core within {secs}s"),
                "raise `timeout_sec`",
            )
        } else if stderr.contains("Python scripting is not supported") {
            not_run(
                "this machine's gdb was built without Python, which debug_core uses to read the stacks",
                "install the distribution's gdb package (it includes Python), e.g. `sudo apt install gdb`",
            )
        } else if stderr.contains("not installed")
            || stderr.contains("No such file or directory (os error 2)")
        {
            not_run(
                "gdb is not installed here",
                "install gdb (e.g. `sudo apt install gdb`)",
            )
        } else {
            not_run(
                &format!(
                    "gdb could not read the core: {}",
                    answer
                        .error
                        .clone()
                        .unwrap_or_else(|| tail_lines(stderr, 3))
                ),
                "pass the exact `program` that crashed, and `sysroot` for a core from another machine",
            )
        };
        text.push_str("\nbacktrace: ");
        text.push_str(&reason);
    }
    text
}

#[cfg(all(test, unix))]
#[path = "core_triage_tests.rs"]
mod tests;
