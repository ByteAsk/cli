//! What a Linux ELF core had mapped, for debugging it against a `sysroot`.
//!
//! A core from another machine (a production Linux box, read on a developer's
//! Mac) names its shared libraries by the paths they had THERE. With a copy of
//! those files under a sysroot, gdb resolves them itself (`set sysroot`). lldb
//! does not: it leaves every library frame as a bare address and cannot unwind
//! through it. It can be told explicitly, though — `target modules add` plus
//! `target modules load --slide` at the address the core recorded — and the
//! core's `NT_FILE` note records exactly that: each mapped file and where.

use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const ET_CORE: u16 = 4;
/// `NT_FILE`, the note listing file-backed mappings ("FILE" in ASCII).
const NT_FILE: u32 = 0x4649_4c45;
/// Upper bound on one note segment read into memory.
const MAX_NOTE_BYTES: u64 = 64 << 20;

/// A file the core had mapped, with the address of its first byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedFile {
    pub path: String,
    pub base: u64,
}

/// Where each sysroot file must be loaded to match the core.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SysrootPlan {
    /// `(file under the sysroot, slide)`: load the file with this slide.
    pub modules: Vec<(PathBuf, u64)>,
    /// Shared libraries the core mapped that the sysroot does not have.
    pub missing: Vec<String>,
}

impl SysrootPlan {
    /// One line for the model: what was found and what was not.
    pub fn describe(&self, sysroot: &Path) -> String {
        let mut line = format!(
            "sysroot {}: {} mapped file(s) found and loaded at the core's addresses",
            sysroot.display(),
            self.modules.len()
        );
        if !self.missing.is_empty() {
            let shown: Vec<&str> = self.missing.iter().take(5).map(String::as_str).collect();
            line.push_str(&format!(
                "; {} shared librar{} NOT in the sysroot (their frames stay unresolved): {}{}",
                self.missing.len(),
                if self.missing.len() == 1 { "y" } else { "ies" },
                shown.join(", "),
                if self.missing.len() > shown.len() {
                    ", …"
                } else {
                    ""
                }
            ));
        }
        line
    }
}

/// Read the files a core had mapped from its `NT_FILE` note, lowest address
/// first. Only 64-bit little-endian cores (x86-64, aarch64) are read.
pub fn mapped_files(core: &Path) -> Result<Vec<MappedFile>, String> {
    let mut file = File::open(core).map_err(|e| format!("cannot open {}: {e}", core.display()))?;
    let header = read_header(&mut file, core)?;
    if header.e_type != ET_CORE {
        return Err(format!(
            "{} is an ELF file but not a core dump",
            core.display()
        ));
    }
    let mut found = Vec::new();
    for ph in program_headers(&mut file, &header)? {
        if ph.p_type != PT_NOTE || ph.filesz == 0 {
            continue;
        }
        if ph.filesz > MAX_NOTE_BYTES {
            return Err(format!(
                "a note segment in {} is implausibly large",
                core.display()
            ));
        }
        let mut notes = vec![0u8; ph.filesz as usize];
        file.seek(SeekFrom::Start(ph.offset)).map_err(io_err)?;
        file.read_exact(&mut notes).map_err(io_err)?;
        found.extend(parse_notes(&notes));
    }
    found.sort_by_key(|m| m.base);
    Ok(found)
}

/// Match every shared object the core mapped with its copy under `sysroot`.
/// `program` (the executable the debugger already loads) is skipped.
pub fn sysroot_plan(
    core: &Path,
    sysroot: &Path,
    program: Option<&Path>,
) -> Result<SysrootPlan, String> {
    let program = program.and_then(|p| p.canonicalize().ok());
    let mut plan = SysrootPlan::default();
    let mut seen = std::collections::BTreeSet::new();
    for mapped in mapped_files(core)? {
        if !mapped.path.starts_with('/') || !seen.insert(mapped.path.clone()) {
            continue;
        }
        let host = sysroot.join(mapped.path.trim_start_matches('/'));
        if !host.is_file() {
            if looks_like_a_library(&mapped.path) {
                plan.missing.push(mapped.path);
            }
            continue;
        }
        if program.is_some() && host.canonicalize().ok() == program {
            continue;
        }
        // Not ELF (a locale archive, a cache file): mapped, but not code.
        let Ok(first_load) = first_load_address(&host) else {
            continue;
        };
        plan.modules
            .push((host, mapped.base.wrapping_sub(first_load)));
    }
    Ok(plan)
}

/// lldb commands that load the plan's modules. A path lldb's command parser
/// cannot quote (it contains `"`) is left out.
pub fn lldb_load_commands(plan: &SysrootPlan) -> Vec<String> {
    let mut commands = Vec::new();
    for (path, slide) in &plan.modules {
        let path = path.to_string_lossy();
        if path.contains('"') {
            continue;
        }
        commands.push(format!("target modules add \"{path}\""));
        commands.push(format!(
            "target modules load --file \"{path}\" --slide {slide:#x}"
        ));
    }
    commands
}

fn looks_like_a_library(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".so") || name.contains(".so.")
}

/// The page-aligned virtual address of the segment holding file offset 0,
/// which is what the core's mapping of the file's first byte corresponds to.
fn first_load_address(path: &Path) -> Result<u64, String> {
    let mut file = File::open(path).map_err(io_err)?;
    let header = read_header(&mut file, path)?;
    program_headers(&mut file, &header)?
        .into_iter()
        .find(|ph| ph.p_type == PT_LOAD && ph.offset < ph.align.max(4096))
        .map(|ph| ph.vaddr & !(ph.align.max(4096) - 1))
        .ok_or_else(|| format!("{} has no loadable segment", path.display()))
}

struct Header {
    e_type: u16,
    phoff: u64,
    phentsize: u16,
    phnum: u16,
}

struct ProgramHeader {
    p_type: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    align: u64,
}

fn read_header(file: &mut File, path: &Path) -> Result<Header, String> {
    let mut h = [0u8; 64];
    file.read_exact(&mut h)
        .map_err(|_| format!("{} is not an ELF file", path.display()))?;
    if &h[0..4] != b"\x7fELF" {
        return Err(format!("{} is not an ELF file", path.display()));
    }
    if h[4] != 2 || h[5] != 1 {
        return Err(format!(
            "{} is not a 64-bit little-endian ELF file, which is all this reads",
            path.display()
        ));
    }
    Ok(Header {
        e_type: u16_at(&h, 16),
        phoff: u64_at(&h, 32),
        phentsize: u16_at(&h, 54),
        phnum: u16_at(&h, 56),
    })
}

fn program_headers(file: &mut File, header: &Header) -> Result<Vec<ProgramHeader>, String> {
    let size = header.phentsize as usize;
    if size < 56 {
        return Err("malformed ELF program header table".to_string());
    }
    let mut table = vec![0u8; size * header.phnum as usize];
    file.seek(SeekFrom::Start(header.phoff)).map_err(io_err)?;
    file.read_exact(&mut table).map_err(io_err)?;
    Ok(table
        .chunks_exact(size)
        .map(|ph| ProgramHeader {
            p_type: u32_at(ph, 0),
            offset: u64_at(ph, 8),
            vaddr: u64_at(ph, 16),
            filesz: u64_at(ph, 32),
            align: u64_at(ph, 48),
        })
        .collect())
}

/// Every `NT_FILE` entry mapped from file offset 0, from one note segment.
fn parse_notes(notes: &[u8]) -> Vec<MappedFile> {
    let mut found = Vec::new();
    let mut at = 0usize;
    while at + 12 <= notes.len() {
        let namesz = u32_at(notes, at) as usize;
        let descsz = u32_at(notes, at + 4) as usize;
        let kind = u32_at(notes, at + 8);
        let desc_at = at + 12 + align4(namesz);
        let Some(desc) = notes.get(desc_at..desc_at.saturating_add(descsz)) else {
            break;
        };
        if kind == NT_FILE {
            found.extend(parse_nt_file(desc));
        }
        at = desc_at + align4(descsz);
    }
    found
}

/// `NT_FILE` descriptor: count, page size, count x (start, end, file offset
/// in pages), then count NUL-terminated file names.
fn parse_nt_file(desc: &[u8]) -> Vec<MappedFile> {
    if desc.len() < 16 {
        return Vec::new();
    }
    let count = u64_at(desc, 0) as usize;
    let names_at = count.checked_mul(24).and_then(|n| n.checked_add(16));
    let Some(names_at) = names_at.filter(|&n| n <= desc.len()) else {
        return Vec::new();
    };
    let names = desc[names_at..].split(|&b| b == 0);
    (0..count)
        .zip(names)
        .filter_map(|(i, name)| {
            let entry = 16 + i * 24;
            let start = u64_at(desc, entry);
            let file_page = u64_at(desc, entry + 16);
            (file_page == 0).then(|| MappedFile {
                path: String::from_utf8_lossy(name).into_owned(),
                base: start,
            })
        })
        .collect()
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

fn io_err(err: std::io::Error) -> String {
    err.to_string()
}

/// A real Linux core "from another machine": a program whose crash is inside a
/// shared library, core-dumped with gdb, after which the library is moved out
/// of its original path into a sysroot, as it is when a production core is read
/// on a developer's box. Needs `cc` and `gdb`.
#[cfg(test)]
pub(crate) mod foreign_core {
    use std::path::PathBuf;

    pub(crate) struct ForeignCore {
        pub _dir: tempfile::TempDir,
        pub program: PathBuf,
        pub core: PathBuf,
        pub sysroot: PathBuf,
    }

    pub(crate) fn build() -> Option<ForeignCore> {
        use crate::program_args::fixture::have;
        if !have("cc") || !have("gdb") {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let root = dir.path().canonicalize().ok()?;
        let lib_dir = root.join("app/lib");
        std::fs::create_dir_all(&lib_dir).ok()?;
        std::fs::write(
            root.join("q.c"),
            "#include <stddef.h>\nstruct order { long id; long qty; };\n\
             long queue_find(struct order *v, size_t n, long id) \
             { for (size_t i = 0;; i++) if (v[i].id == id) return v[i].qty; }\n",
        )
        .ok()?;
        std::fs::write(
            root.join("m.c"),
            "#include <stddef.h>\nstruct order { long id; long qty; };\n\
             long queue_find(struct order *v, size_t n, long id);\n\
             int main(void) { return (int)queue_find((struct order *)0x7f0000000004, 5, 42); }\n",
        )
        .ok()?;
        let cc = |args: &[&str]| {
            std::process::Command::new("cc")
                .current_dir(&root)
                .args(args)
                .status()
                .ok()?
                .success()
                .then_some(())
        };
        cc(&[
            "-g",
            "-O0",
            "-fPIC",
            "-shared",
            "-o",
            "app/lib/libqueue.so",
            "q.c",
        ])?;
        let rpath = format!("-Wl,-rpath,{}", lib_dir.display());
        cc(&[
            "-g",
            "-O0",
            "-o",
            "trader",
            "m.c",
            "-Lapp/lib",
            "-lqueue",
            &rpath,
        ])?;
        let core = root.join("core.trader");
        let status = std::process::Command::new("gdb")
            .args(["-q", "-nx", "-batch", "-ex", "run", "-ex"])
            .arg(format!("generate-core-file {}", core.display()))
            .arg(root.join("trader"))
            .output()
            .ok()?;
        if !core.is_file() {
            eprintln!(
                "skip: gdb made no core: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            return None;
        }
        // The library leaves its original path: only the sysroot has it now.
        let sysroot = root.join("sysroot");
        let moved = sysroot.join(lib_dir.strip_prefix("/").ok()?);
        std::fs::create_dir_all(&moved).ok()?;
        std::fs::rename(lib_dir.join("libqueue.so"), moved.join("libqueue.so")).ok()?;
        Some(ForeignCore {
            program: root.join("trader"),
            core,
            sysroot,
            _dir: dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_real_core_maps_the_moved_library_to_the_address_it_had() {
        let Some(fixture) = foreign_core::build() else {
            eprintln!("skip: needs cc and gdb");
            return;
        };
        let mapped = mapped_files(&fixture.core).expect("parse");
        let lib = mapped
            .iter()
            .find(|m| m.path.ends_with("/app/lib/libqueue.so"))
            .expect("the core recorded the library");
        let plan =
            sysroot_plan(&fixture.core, &fixture.sysroot, Some(&fixture.program)).expect("plan");
        let (path, slide) = plan
            .modules
            .iter()
            .find(|(p, _)| p.ends_with("app/lib/libqueue.so"))
            .expect("found under the sysroot");
        assert!(path.starts_with(&fixture.sysroot), "{}", path.display());
        // A shared object's first segment is linked at 0: the slide IS the base.
        assert_eq!(*slide, lib.base);
        assert!(
            plan.modules.iter().all(|(p, _)| p != &fixture.program),
            "program skipped"
        );
    }

    /// A minimal ELF core: header, one PT_NOTE, and a note holding `NT_FILE`.
    fn synthetic_core(entries: &[(u64, u64, u64, &str)]) -> Vec<u8> {
        let mut desc = Vec::new();
        desc.extend((entries.len() as u64).to_le_bytes());
        desc.extend(4096u64.to_le_bytes());
        for (start, end, page, _) in entries {
            desc.extend(start.to_le_bytes());
            desc.extend(end.to_le_bytes());
            desc.extend(page.to_le_bytes());
        }
        for (.., name) in entries {
            desc.extend(name.as_bytes());
            desc.push(0);
        }
        let mut note = Vec::new();
        // An unrelated note first, so the walk has to step over one.
        note.extend(5u32.to_le_bytes());
        note.extend(4u32.to_le_bytes());
        note.extend(1u32.to_le_bytes());
        note.extend(b"CORE\0\0\0\0");
        note.extend([9u8; 4]);
        note.extend(5u32.to_le_bytes());
        note.extend((desc.len() as u32).to_le_bytes());
        note.extend(NT_FILE.to_le_bytes());
        note.extend(b"CORE\0\0\0\0");
        note.extend(&desc);
        while note.len() % 4 != 0 {
            note.push(0);
        }
        let mut elf = vec![0u8; 64];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[16..18].copy_from_slice(&ET_CORE.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        let mut ph = vec![0u8; 56];
        ph[0..4].copy_from_slice(&PT_NOTE.to_le_bytes());
        ph[8..16].copy_from_slice(&120u64.to_le_bytes());
        ph[32..40].copy_from_slice(&(note.len() as u64).to_le_bytes());
        elf.extend(ph);
        elf.extend(note);
        elf
    }

    #[test]
    fn reads_the_first_mapping_of_every_file_from_nt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let core = dir.path().join("core");
        std::fs::write(
            &core,
            synthetic_core(&[
                (0x7f00_0000_1000, 0x7f00_0000_2000, 1, "/lib/libc.so.6"),
                (0x7f00_0000_0000, 0x7f00_0000_1000, 0, "/lib/libc.so.6"),
                (0x5555_0000_0000, 0x5555_0000_1000, 0, "/opt/app/trader"),
            ]),
        )
        .expect("write");
        assert_eq!(
            mapped_files(&core).expect("parse"),
            vec![
                MappedFile {
                    path: "/opt/app/trader".to_string(),
                    base: 0x5555_0000_0000
                },
                MappedFile {
                    path: "/lib/libc.so.6".to_string(),
                    base: 0x7f00_0000_0000
                },
            ]
        );
    }

    #[test]
    fn a_truncated_or_foreign_file_is_an_error_or_empty_never_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_elf = dir.path().join("not-elf");
        std::fs::write(&not_elf, b"hello").expect("write");
        assert!(mapped_files(&not_elf).is_err());
        // A count that claims more entries than the descriptor holds.
        let mut bogus = Vec::new();
        bogus.extend(u64::MAX.to_le_bytes());
        bogus.extend(4096u64.to_le_bytes());
        assert_eq!(parse_nt_file(&bogus), Vec::new());
        assert_eq!(parse_notes(&[1, 2, 3]), Vec::new());
    }

    #[test]
    fn a_library_missing_from_the_sysroot_is_named_and_a_data_file_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let core = dir.path().join("core");
        std::fs::write(
            &core,
            synthetic_core(&[
                (0x7f00_0000_0000, 0x7f00_0000_1000, 0, "/lib/libgone.so.1"),
                (
                    0x7f00_1000_0000,
                    0x7f00_1000_1000,
                    0,
                    "/usr/lib/locale/locale-archive",
                ),
            ]),
        )
        .expect("write");
        let sysroot = dir.path().join("sysroot");
        std::fs::create_dir_all(&sysroot).expect("mkdir");
        let plan = sysroot_plan(&core, &sysroot, None).expect("plan");
        assert_eq!(plan.missing, vec!["/lib/libgone.so.1".to_string()]);
        assert!(plan.modules.is_empty());
        assert!(plan.describe(&sysroot).contains("NOT in the sysroot"));
    }

    #[test]
    fn lldb_commands_quote_paths_and_skip_ones_they_cannot_quote() {
        let plan = SysrootPlan {
            modules: vec![
                (PathBuf::from("/s/lib/a b.so"), 0x7f00_0000_0000),
                (PathBuf::from("/s/lib/q\"uote.so"), 0x1000),
            ],
            missing: Vec::new(),
        };
        assert_eq!(
            lldb_load_commands(&plan),
            vec![
                "target modules add \"/s/lib/a b.so\"".to_string(),
                "target modules load --file \"/s/lib/a b.so\" --slide 0x7f0000000000".to_string(),
            ]
        );
    }
}
