# ByteAsk debug_core driver. Runs INSIDE gdb, on a core file (program + core
# already loaded on the command line). Fixed: the model never supplies gdb
# commands. Output: JSON lines in $BYTEASK_CORE_OUT.

import json
import os
import re

import gdb

QUERY = json.loads(os.environ.get("BYTEASK_CORE_QUERY", "{}"))
OUT = open(os.environ["BYTEASK_CORE_OUT"], "a", buffering=1)

MAX_VALUE = 200
SYSTEM_PREFIXES = ("/usr/", "/lib/", "/lib64/", "/opt/rh/", "/nix/store/")
REGS = ("rip", "rsp", "rbp", "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10",
        "r11", "r12", "r13", "r14", "r15", "pc", "sp", "x0", "x1", "x2", "x3", "x8", "x29", "x30")


def emit(kind, **fields):
    fields["kind"] = kind
    OUT.write(json.dumps(fields) + "\n")
    OUT.flush()


def short(text):
    text = " ".join(str(text).split())
    return text if len(text) <= MAX_VALUE else text[: MAX_VALUE - 3] + "..."


def roots():
    return [r.rstrip("/") + "/" for r in QUERY.get("user_roots", []) if r]


def is_user(rec):
    path = rec.get("file")
    if not path:
        return False
    if any(path.startswith(r) for r in roots()):
        return True
    if path.startswith(SYSTEM_PREFIXES):
        return False
    lib = rec.get("library")
    if lib and lib.startswith(SYSTEM_PREFIXES):
        return False
    if roots() and not os.path.exists(path):
        return False
    return True


def args_of(frame):
    out = []
    try:
        block = frame.block()
    except RuntimeError:
        return out
    while block is not None and block.function is None:
        block = block.superblock
    if block is None:
        return out
    for sym in block:
        if sym.is_argument:
            try:
                out.append([sym.name, short(sym.value(frame))])
            except Exception as err:  # noqa: BLE001
                out.append([sym.name, "<unreadable: %s>" % short(err)])
    return out


def frame_record(frame, level, with_args):
    rec = {"level": level, "pc": "0x%x" % frame.pc()}
    if frame.name():
        rec["function"] = frame.name()
    try:
        sal = frame.find_sal()
    except gdb.error:
        sal = None
    if sal is not None and sal.symtab is not None:
        rec["file"] = sal.symtab.fullname()
        rec["line"] = sal.line
    try:
        lib = gdb.solib_name(frame.pc())
    except gdb.error:
        lib = None
    if lib:
        rec["library"] = lib
    rec["user"] = is_user(rec)
    if with_args and rec["user"]:
        rec["args"] = args_of(frame)
    return rec


def frames(limit, with_args):
    out = []
    try:
        frame = gdb.newest_frame()
    except gdb.error:
        return out, False
    level = 0
    while frame is not None and level < limit:
        out.append(frame_record(frame, level, with_args))
        try:
            frame = frame.older()
        except gdb.error:
            frame = None
        level += 1
    return out, frame is not None


def registers():
    out = {}
    frame = gdb.newest_frame()
    for name in REGS:
        try:
            out[name] = "0x%x" % (int(frame.read_register(name)) & 0xFFFFFFFFFFFFFFFF)
        except (gdb.error, ValueError):
            continue
    return out


def instruction():
    try:
        frame = gdb.newest_frame()
        insn = frame.architecture().disassemble(frame.pc(), count=1)[0]
        return insn["asm"]
    except (gdb.error, IndexError):
        return None


def siginfo():
    try:
        info = gdb.parse_and_eval("$_siginfo")
        rec = {"signo": int(info["si_signo"]), "code": int(info["si_code"])}
        try:
            rec["addr"] = "0x%x" % (int(info["_sifields"]["_sigfault"]["si_addr"]) & 0xFFFFFFFFFFFFFFFF)
        except gdb.error:
            pass
        try:
            rec["pid"] = int(info["_sifields"]["_kill"]["si_pid"])
        except gdb.error:
            pass
        return rec
    except gdb.error:
        return None


def c_string(expr, limit=400):
    try:
        text = gdb.parse_and_eval(expr).string(errors="replace", length=limit)
    except (gdb.error, UnicodeDecodeError, RuntimeError):
        return None
    text = text.split("\x00", 1)[0].strip()
    return text or None


def abort_message():
    """glibc keeps the text of its last fatal message (an assert, a malloc
    check, a fortify failure) in `__abort_msg` precisely so a core holds it."""
    for expr in ("__abort_msg->msg", "(char *)(*(char **)&__abort_msg) + 4"):
        text = c_string(expr)
        if text:
            return short(text)
    return None


# The C++ exception class tag at the start of an _Unwind_Exception header:
# "GNUCC++\0" (a thrown exception) or "GNUCC++\x01" (a dependent one).
GNU_CXX_CLASSES = (0x474E5543432B2B00, 0x474E5543432B2B01)
CALLEE_SAVED = ("rbx", "rbp", "r12", "r13", "r14", "r15",
                "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28")


def symbol_at(addr):
    try:
        return gdb.execute("info symbol 0x%x" % addr, to_string=True).strip()
    except gdb.error:
        return ""


def exception_info(thread_frames):
    """For a terminate during a throw: the exception's type and what(), read
    from the core. __cxa_throw keeps the exception header in a callee-saved
    register while unwinding; the thrown object follows the 32-byte header."""
    level = next((f["level"] for f in thread_frames
                  if f.get("function") in ("__cxa_throw", "__cxa_rethrow")), None)
    if level is None:
        return None
    frame = gdb.newest_frame()
    for _ in range(level):
        frame = frame.older()
    info = {"frame": level}
    inferior = gdb.selected_inferior()
    for reg in CALLEE_SAVED:
        try:
            value = int(frame.read_register(reg)) & 0xFFFFFFFFFFFFFFFF
        except (gdb.error, ValueError):
            continue
        if "type" not in info:
            m = re.match(r"typeinfo for (.+?)(@\S+)? in section", symbol_at(value))
            if m:
                info["type"] = m.group(1)
        try:
            head = int.from_bytes(bytes(inferior.read_memory(value, 8)), "little")
        except (gdb.error, gdb.MemoryError, ValueError, OverflowError):
            continue
        if head not in GNU_CXX_CLASSES:
            continue
        obj = value + 0x20
        try:
            vptr = int.from_bytes(bytes(inferior.read_memory(obj, 8)), "little")
        except (gdb.error, gdb.MemoryError):
            break
        m = re.match(r"vtable for (.+?)(@\S+)? \+ \d+ in section", symbol_at(vptr))
        if m:
            info["type"] = m.group(1)
            # std::logic_error / std::runtime_error and everything derived from
            # them keep the what() text behind the vtable pointer.
            what = c_string("*(char **)0x%x" % (obj + 8), 300)
            if what and all(ch.isprintable() for ch in what):
                info["what"] = short(what)
        break
    return info if len(info) > 1 else None


def main():
    for setting in ("pagination off", "confirm off", "width 0", "print pretty off",
                    "print elements 32", "print frame-arguments scalars", "may-call-functions off"):
        try:
            gdb.execute("set " + setting, to_string=True)
        except gdb.error:
            pass
    inferior = gdb.selected_inferior()
    selected = gdb.selected_thread()
    if selected is None:
        emit("error", message="gdb did not load the core (no threads)")
        return
    own, more = frames(int(QUERY.get("faulting_frames", 256)), True)
    emit("core", tid=selected.ptid[1] or selected.ptid[0], siginfo=siginfo(),
         instruction=instruction(), registers=registers(), abort_msg=abort_message(),
         exception=exception_info(own), threads=len(inferior.threads()))
    emit("thread", tid=selected.ptid[1] or selected.ptid[0], faulting=True, frames=own, truncated=more)
    shown = 0
    for thread in sorted(inferior.threads(), key=lambda t: t.num):
        if thread.ptid == selected.ptid:
            continue
        if shown >= int(QUERY.get("max_threads", 64)):
            break
        try:
            thread.switch()
            recs, more = frames(int(QUERY.get("other_frames", 48)), False)
        except gdb.error:
            continue
        emit("thread", tid=thread.ptid[1] or thread.ptid[0], faulting=False, frames=recs, truncated=more)
        shown += 1
    selected.switch()
    emit("done")


try:
    main()
except Exception as err:  # noqa: BLE001 - every failure must reach the Rust side as data
    emit("error", message=short("%s: %s" % (type(err).__name__, err)))
finally:
    OUT.close()
