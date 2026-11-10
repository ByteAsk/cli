# ByteAsk debug_reverse driver. Runs INSIDE gdb, inside `rr replay`.
#
# This file is fixed: the model never supplies gdb commands. Its inputs arrive
# as one JSON object in $BYTEASK_RR_QUERY and its answer leaves as JSON lines in
# the file named by $BYTEASK_RR_OUT, one line per fact, written and flushed as
# soon as it is known, so a query cut off by the time limit still leaves what it
# found (and the Rust side can say it is incomplete).
#
# Every question is answered by running time BACKWARDS from a stop point over a
# deterministic recording. Nothing here re-runs the live program.

import json
import os
import re

import gdb

QUERY = json.loads(os.environ.get("BYTEASK_RR_QUERY", "{}"))
OUT = open(os.environ["BYTEASK_RR_OUT"], "a", buffering=1)

MAX_FRAMES = 12
MAX_THREADS = 64
MAX_VALUE = 240
SYSTEM_PREFIXES = ("/usr/", "/lib/", "/lib64/", "/opt/rh/", "/nix/store/")

_last_stop = [None]


def _on_stop(event):
    _last_stop[0] = event


gdb.events.stop.connect(_on_stop)


def emit(kind, **fields):
    fields["kind"] = kind
    OUT.write(json.dumps(fields) + "\n")
    OUT.flush()


def run(command):
    """Run one gdb command that does not move the program. Raises gdb.error."""
    return gdb.execute(command, to_string=True)


def move(command):
    """Run a command that moves the replay (continue, reverse-continue, ...).
    The stop event it causes is the only evidence of WHY it stopped, so it is
    cleared first and read straight after."""
    _last_stop[0] = None
    return gdb.execute(command, to_string=True)


def short(text):
    text = " ".join(str(text).split())
    return text if len(text) <= MAX_VALUE else text[: MAX_VALUE - 3] + "..."


def event_number():
    try:
        m = re.search(r"Completed event:\s*(\d+)", run("when"))
        return int(m.group(1)) if m else None
    except gdb.error:
        return None


def user_roots():
    return [r.rstrip("/") + "/" for r in QUERY.get("user_roots", []) if r]


def frame_record(frame, level):
    rec = {"level": level, "pc": "0x%x" % frame.pc()}
    name = frame.name()
    if name:
        rec["function"] = name
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
    return rec


def is_user(rec):
    path = rec.get("file")
    if not path:
        return False
    roots = user_roots()
    if roots and any(path.startswith(r) for r in roots):
        return True
    if path.startswith(SYSTEM_PREFIXES):
        return False
    lib = rec.get("library")
    if lib and lib.startswith(SYSTEM_PREFIXES):
        return False
    # Sources gdb cannot find that were compiled elsewhere (libc's
    # ../sysdeps/..., ./nptl/...) resolve under a build dir: treat a missing
    # file outside the user's roots as not theirs.
    if roots and not os.path.exists(path):
        return False
    return True


def frames_of_selected_thread(limit=MAX_FRAMES):
    out = []
    frame = gdb.newest_frame()
    level = 0
    while frame is not None and level < limit:
        out.append(frame_record(frame, level))
        try:
            frame = frame.older()
        except gdb.error:
            break
        level += 1
    return out


def thread_record(thread):
    pid, lwp, _ = thread.ptid
    return {"num": thread.num, "tid": lwp or pid, "name": thread.name or ""}


def select_user_frame():
    """Select the innermost frame in the user's own code, as an engineer types
    `up` out of libc. Returns its level, or None when there is none."""
    frame = gdb.newest_frame()
    level = 0
    while frame is not None and level < 64:
        if is_user(frame_record(frame, level)):
            frame.select()
            return level
        try:
            frame = frame.older()
        except gdb.error:
            break
        level += 1
    return None


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
            except Exception as err:  # noqa: BLE001 - an unreadable arg is reported, not fatal
                out.append([sym.name, "<unreadable: %s>" % short(err)])
    return out


def stop_description():
    event = _last_stop[0]
    rec = {}
    if isinstance(event, gdb.SignalEvent):
        rec["signal"] = event.stop_signal
    elif isinstance(event, gdb.BreakpointEvent):
        rec["breakpoint"] = True
    return rec


def stopped_at_end_of_recording(frames):
    """rr ends a replay with a synthetic SIGKILL in its own syscall page."""
    sig = stop_description().get("signal")
    top = frames[0].get("function", "") if frames else ""
    return sig == "SIGKILL" and (top == "syscall_traced" or not frames or "function" not in frames[0])


def quiet_signals():
    # From the stop point on, only OUR breakpoints and watchpoints may stop a
    # reverse run: a signal stop would otherwise be mistaken for an answer.
    for spec in ("all", "SIGKILL"):
        try:
            run("handle %s nostop noprint" % spec)
        except gdb.error:
            pass


def go_to_stop_point():
    """Move to the point the question is asked FROM. Returns a dict, or None
    after emitting an error."""
    stop_at = QUERY.get("stop_at")
    fatal = QUERY.get("fatal_signal")
    if QUERY.get("started_at_event") is not None:
        how = "event"
    elif stop_at:
        hit = max(1, int(QUERY.get("stop_hit") or 1))
        quiet_signals()
        try:
            bp = gdb.Breakpoint(stop_at, internal=False)
        except gdb.error as err:
            emit("error", where="stop_at", message=short(err))
            return None
        if bp.pending or not bp.locations:
            emit("error", where="stop_at", message="no code location matches %r" % stop_at)
            return None
        bp.ignore_count = hit - 1
        try:
            move("continue")
        except gdb.error as err:
            emit("error", where="stop_at", message=short(err))
            return None
        ev = _last_stop[0]
        if not (isinstance(ev, gdb.BreakpointEvent) and bp in ev.breakpoints):
            emit("error", where="stop_at",
                 message="the recording ended before %r was reached %d time(s) (it was reached %d time(s))"
                 % (stop_at, hit, bp.hit_count))
            return None
        bp.delete()
        how = "stop_at"
    elif fatal:
        # The process died on this signal: stop where it was delivered.
        quiet_signals()
        try:
            run("handle %s stop print" % fatal)
        except gdb.error:
            pass
        try:
            move("continue")
        except gdb.error as err:
            emit("error", where="stop", message=short(err))
            return None
        how = "signal"
    else:
        how = "end"
    frames = frames_of_selected_thread()
    rec = {"how": how, "event": event_number(), "frames": frames}
    rec.update(stop_description())
    wanted = QUERY.get("started_at_event")
    if how == "event" and wanted is not None and rec["event"] is not None and rec["event"] > wanted:
        rec["note"] = ("rr cannot stop exactly at event %d; this is event %d, the first point after it where "
                       "it can stop" % (wanted, rec["event"]))
    if how == "signal" and rec.get("signal") != fatal:
        if stopped_at_end_of_recording(frames):
            rec["how"] = "end"
        else:
            rec["note"] = "stopped on %s, expected %s" % (rec.get("signal"), fatal)
    try:
        rec["thread"] = thread_record(gdb.selected_thread())
    except Exception:  # noqa: BLE001
        pass
    quiet_signals()
    return rec


def all_threads():
    out = []
    current = gdb.selected_thread()
    for thread in list(gdb.selected_inferior().threads())[:MAX_THREADS]:
        try:
            thread.switch()
            rec = thread_record(thread)
            rec["frames"] = frames_of_selected_thread(8)
            out.append(rec)
        except gdb.error:
            continue
    if current is not None:
        current.switch()
    return out


def stopped_on(bp):
    ev = _last_stop[0]
    return isinstance(ev, gdb.BreakpointEvent) and bp in ev.breakpoints


def watch_target(expr):
    value = gdb.parse_and_eval(expr)
    if value.address is None:
        raise gdb.error("`%s` is not something stored in memory (not an lvalue), so it cannot be watched" % expr)
    return int(value.address), value.type


def read_at(addr, typ):
    try:
        return short(gdb.Value(addr).cast(typ.pointer()).dereference())
    except gdb.error as err:
        return "<unreadable: %s>" % short(err)


def who_wrote(count):
    expr = QUERY["expr"]
    level = select_user_frame()
    try:
        addr, typ = watch_target(expr)
    except gdb.error as err:
        emit("error", where="expr", message=short(err))
        return
    size = typ.sizeof
    emit("target", expr=expr, address="0x%x" % addr, size=size, type=str(typ),
         frame_level=level, value=read_at(addr, typ))
    if size > 32:
        emit("error", where="expr",
             message="`%s` is %d bytes; hardware watchpoints cover at most 32 bytes. Watch one member" % (expr, size))
        return
    text = run("watch -l %s" % expr)
    bp = gdb.breakpoints()[-1]
    if "Hardware" not in text:
        bp.delete()
        emit("error", where="expr",
             message="gdb could not place a hardware watchpoint on `%s` (%s)" % (expr, short(text)))
        return
    for _ in range(count):
        after = read_at(addr, typ)
        try:
            move("reverse-continue")
        except gdb.error as err:
            emit("error", where="reverse", message=short(err))
            return
        if not stopped_on(bp):
            emit("start", value=read_at(addr, typ))
            return
        before = read_at(addr, typ)
        emit("write", event=event_number(), thread=thread_record(gdb.selected_thread()),
             frames=frames_of_selected_thread(), before=before, after=after)


def last_reached(count):
    location = QUERY["location"]
    try:
        bp = gdb.Breakpoint(location, internal=False)
    except gdb.error as err:
        emit("error", where="location", message=short(err))
        return
    if bp.pending or not bp.locations:
        emit("error", where="location", message="no code location matches %r" % location)
        return
    for _ in range(count):
        try:
            move("reverse-continue")
        except gdb.error as err:
            emit("error", where="reverse", message=short(err))
            return
        if not stopped_on(bp):
            emit("start")
            return
        frame = gdb.newest_frame()
        emit("hit", event=event_number(), thread=thread_record(gdb.selected_thread()),
             frames=frames_of_selected_thread(), args=args_of(frame))


def call_site(frame):
    """Address of the call instruction a caller frame is suspended in (its pc
    is the RETURN address, one instruction later), or None."""
    ret = frame.pc()
    arch = frame.architecture()
    lengths = (4,) if "aarch64" in arch.name() else (5, 6, 2, 3, 7, 4)
    for length in lengths:
        try:
            insn = arch.disassemble(ret - length, count=1)[0]
        except (gdb.error, IndexError):
            continue
        if insn["length"] == length and insn["asm"].split()[0].startswith(("call", "bl")):
            return ret - length
    return None


def back_to_call(caller):
    """Run backwards to the moment `caller` made the call it is still inside.
    `reverse-finish` cannot do this from a signal stop: it finishes the newest
    REAL frame, which is rr's own syscall hook, not the frame selected."""
    pc = call_site(caller)
    if pc is None:
        return "could not find the call instruction in %s" % (caller.name() or "your code")
    bp = gdb.Breakpoint("*0x%x" % pc, internal=True)
    try:
        bp.thread = gdb.selected_thread().num
        move("reverse-continue")
        if not stopped_on(bp):
            return "could not step back out of the library into your code"
    except gdb.error as err:
        return short(err)
    finally:
        bp.delete()
    return None


def last_lines(count):
    # Climb out of libc/abort into the user's own frame, BACKWARDS, so the
    # lines walked are the user's code leading up to the stop.
    level = select_user_frame()
    if level is None:
        emit("error", where="reverse", message="no frame of your own code is on the stopped thread")
        return
    if level > 0:
        err = back_to_call(gdb.selected_frame())
        if err:
            emit("error", where="reverse", message=err)
            return
    seen = 0
    while seen < count:
        rec = frame_record(gdb.newest_frame(), 0)
        rec["event"] = event_number()
        emit("line", **rec)
        seen += 1
        try:
            text = move("reverse-next")
        except gdb.error as err:
            emit("error", where="reverse", message=short(err))
            return
        if "No more reverse-execution history" in text:
            emit("start")
            return


def main():
    for setting in ("pagination off", "confirm off", "width 0", "print pretty off",
                    "print elements 32", "print frame-arguments scalars", "may-call-functions off"):
        try:
            run("set " + setting)
        except gdb.error:
            pass
    stop = go_to_stop_point()
    if stop is None:
        return
    emit("stop", **stop)
    query = QUERY.get("query")
    count = max(1, int(QUERY.get("count") or 1))
    if query == "stop_point":
        emit("threads", threads=all_threads())
    elif query == "who_wrote":
        who_wrote(count)
    elif query == "last_reached":
        last_reached(count)
    elif query == "last_lines":
        last_lines(count)
    else:
        emit("error", where="query", message="unknown query %r" % query)
        return
    emit("done")


try:
    main()
except Exception as err:  # noqa: BLE001 - every failure must reach the Rust side as data
    emit("error", where="driver", message=short("%s: %s" % (type(err).__name__, err)))
finally:
    OUT.close()
