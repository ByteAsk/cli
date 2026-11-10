#!/usr/bin/env python3
"""Real-gdb integration test for byteask_gdb_bridge.py.

Drives a real gdb over a pty (the human channel) while a socket client (the AI
channel) attaches and exercises the bridge end to end. Skips cleanly (exit 0)
when gdb or gcc is unavailable, mirroring the crate's other tool tests.

    python3 byteask_gdb_bridge_test.py
"""
import glob, json, os, pty, shutil, signal, socket, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
BRIDGE = os.path.join(HERE, "byteask_gdb_bridge.py")

results = []
def check(name, ok):
    results.append(ok)
    print(f"[{'PASS' if ok else 'FAIL'}] {name}")

# ── outside gdb: the history keeps the NEWEST entries ──────────────────
import importlib.util
_spec = importlib.util.spec_from_file_location("byteask_gdb_bridge_standalone", BRIDGE)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)
_h = _mod._History()
for _i in range(_mod.HISTORY_MAX):
    _h.add(f"print long_variable_name_number_{_i:04d}", "agent")
_formatted = _h.format()
check("history keeps the newest entry", f"number_{_mod.HISTORY_MAX - 1:04d}" in _formatted)
check("history says how many older entries it left out", "older entries omitted" in _formatted)
check("history fits the output cap", len(_formatted) <= _mod.OUTPUT_CAP)

if not (shutil.which("gdb") and shutil.which("gcc")):
    print("SKIP: gdb/gcc not available")
    sys.exit(0 if all(results) else 1)

DEMO = r"""
#include <stdio.h>
int add(int a, int b){ int s=a+b; return s; }
int main(void){
    int x=41; int y=add(x,1); printf("y=%d\n", y);
    long acc=0; for(long i=0;i<5000000000L;i++){ acc+=i&7; }
    printf("acc=%ld\n", acc); return 0;
}
"""

work = tempfile.mkdtemp(prefix="byteask-bridge-it-")
src = os.path.join(work, "demo.c"); binp = os.path.join(work, "demo")
open(src, "w").write(DEMO)
open(os.path.join(work, "latin.c"), "wb").write(b"int latin(void){\n  /* caf\xe9 */\n  return 0;\n}\n")
subprocess.run(["gcc", "-g", "-O0", "-o", binp, src, os.path.join(work, "latin.c")], check=True)

home = os.path.join(work, "home"); os.makedirs(home)
env = dict(os.environ, BYTEASK_HOME=home, TERM="dumb", BYTEASK_BRIDGE_REPLY_TIMEOUT="3")
master, slave = pty.openpty()
proc = subprocess.Popen(["gdb", "-q", "-nx", binp], stdin=slave, stdout=slave,
                        stderr=slave, env=env, close_fds=True)
os.close(slave)
buf = bytearray(); lock = threading.Lock()
def _reader():
    while True:
        try:
            d = os.read(master, 4096)
        except OSError:
            break
        if not d:
            break
        with lock:
            buf.extend(d)
threading.Thread(target=_reader, daemon=True).start()
def send(s): os.write(master, (s + "\n").encode())
def text():
    with lock:
        return bytes(buf).decode("utf-8", "replace")
def wait_for(m, t=8):
    end = time.time() + t
    while time.time() < end:
        if m in text():
            return True
        time.sleep(0.05)
    return False

try:
    send("set pagination off"); send("set confirm off")
    send("print 1234567")   # typed BEFORE arming: must not show as this session's history
    send(f"source {BRIDGE}"); send("byteask-bridge")
    check("arm on a tty", wait_for("bridge armed"))
    send("break add"); send("run")
    check("hit breakpoint", wait_for("Breakpoint"))
    time.sleep(0.4)

    metas = glob.glob(os.path.join(home, "gdb-agent", "*.json"))
    check("session discoverable", len(metas) == 1)
    info = json.load(open(metas[0])) if metas else {}
    sock_path = info.get("socket", "")
    check("socket exists", bool(sock_path) and os.path.exists(sock_path))
    check("gdb-agent dir 0700",
          bool(sock_path) and oct(os.stat(os.path.dirname(sock_path)).st_mode & 0o777) == "0o700")

    def ai(cmd, t=30):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(t)
        s.connect(sock_path); s.sendall((cmd + "\n").encode()); s.shutdown(socket.SHUT_WR)
        out = b""
        while True:
            try:
                c = s.recv(4096)
            except socket.timeout:
                break
            if not c:
                break
            out += c
        s.close()
        return out.decode("utf-8", "replace").strip()

    check("__runstate__ stopped", ai("__runstate__") == "stopped")
    bt = ai("bt"); check("auto-run bt shows add+main", "add" in bt and "main" in bt)
    check("auto-run 'print a' == 41", "41" in ai("print a"))
    snap = ai("__snapshot__")
    check("__snapshot__ has frame", "[frame]" in snap)
    check("snapshot echoes to TTY (full transparency, opt A)",
          "[byteask] attaching" in text() and "[byteask] (gdb) bt 10" in text())
    check("history captured user cmds", "break add" in ai("__history__") and "run" in ai("__history__"))
    check("commands typed before arming are not this session's", "1234567" not in ai("__history__"))
    # A Latin-1 source file: gdb's text output raises UnicodeDecodeError on it.
    latin = ai("list latin.c:1,4")
    check("a non-UTF-8 source lists instead of failing", "caf" in latin and "error" not in latin.lower())
    check("transparency echo to TTY", "[byteask] (gdb) bt" in text())

    # A STOPPED program is not signalled (the SIGINT used to stay queued
    # and fire on the next continue).
    check("interrupt refuses a stopped program", "Nothing to interrupt" in ai("__interrupt__"))

    # A client that connects and says nothing must not block others.
    silent = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); silent.connect(sock_path)
    t0 = time.time(); state = ai("__runstate__", t=5)
    check("a silent client does not block the next one", state == "stopped" and time.time() - t0 < 2)
    silent.close()

    # While gdb is busy with the human's own command, an AI command times
    # out — and must then be CANCELLED, never run later.
    send("shell sleep 6"); time.sleep(0.5)
    reply = ai("set var a = a + 1000", t=10)
    check("a command gdb did not pick up says NOT EXECUTED", "NOT EXECUTED" in reply)
    time.sleep(7)
    check("the cancelled command never ran later", "= 41" in ai("print a"))
    check("the cancelled command is not in the history", "a + 1000" not in ai("__history__"))

    send("continue"); time.sleep(1.0)
    check("__runstate__ running after continue", ai("__runstate__") == "running")
    check("state query short-circuits while running", "running" in ai("bt").lower())
    ai("__interrupt__"); time.sleep(1.0)
    check("interrupt pauses (runstate stopped)", ai("__runstate__") == "stopped")
    check("bt works again after interrupt", "main" in ai("bt"))
finally:
    try:
        proc.send_signal(signal.SIGKILL)
    except Exception:
        pass
    os.close(master)
    shutil.rmtree(work, ignore_errors=True)

print(f"\n=== {sum(results)}/{len(results)} checks passed ===")
sys.exit(0 if all(results) else 1)
