#!/usr/bin/env python3
"""Drive the installed engine's real cpp_kdb through a scripted Responses server.

No cloud inference or credentials are used. Start a disposable KGDB/KDB guest
first, then pass its serial proxy port. The script never resumes that guest.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import socket
import struct
import subprocess
import termios
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Responses(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        side = request.get("text", {}).get("format", {}).get("type") == "json_schema"
        index = len(self.server.requests)
        if side:
            self.server.side_requests.append(request)
        else:
            self.server.requests.append(request)
        rid = f"response-title-{len(self.server.side_requests)}" if side else f"response-kdb-{index}"
        events = [{"type": "response.created", "response": {"id": rid}}]
        if not side and index < len(self.server.steps):
            events.append({"type": "response.output_item.done", "item": {
                "type": "function_call", "call_id": f"kdb-{index}",
                "name": "cpp_kdb", "arguments": json.dumps(self.server.steps[index]),
            }})
        else:
            events.append({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant", "id": "message-kdb",
                "content": [{"type": "output_text", "text": json.dumps({"title": "Kernel inspection"}) if side else "KDB fixture completed."}],
            }})
        events.append({"type": "response.completed", "response": {
            "id": rid, "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
        }})
        data = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def provider_args(engine, port):
    return [str(engine), "-c", 'model_provider="kdb-fixture"',
            "-c", 'model_providers.kdb-fixture.name="KDB fixture"',
            "-c", f'model_providers.kdb-fixture.base_url="http://127.0.0.1:{port}/v1"',
            "-c", 'model_providers.kdb-fixture.wire_api="responses"',
            "-c", 'model_providers.kdb-fixture.requires_openai_auth=false',
            "-c", 'model_providers.kdb-fixture.supports_websockets=false',
            "-c", 'mcp_servers={}', "-c", 'features.apps=false']


def scenario(engine, steps, *, trusted=True, json_output=True):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Responses)
    server.steps, server.requests, server.side_requests = steps, [], []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="byteask-kdb-e2e-") as scratch:
            args = provider_args(engine, server.server_port) + [
                    "exec", "--ignore-user-config", "--ignore-rules", "--ephemeral",
                    "--skip-git-repo-check", "--model", "gpt-5.4", "-C", scratch]
            if json_output:
                args.append("--json")
            args.append("Inspect the prepared kernel with cpp_kdb; use only the requested fixture calls.")
            env = dict(os.environ)
            env.pop("BYTEASK_ALLOW_NATIVE_TOOLS", None)
            env["BYTEASK_TOOL_LOG"] = str(Path(scratch) / "tools.jsonl")
            if trusted:
                env["BYTEASK_ALLOW_NATIVE_TOOLS"] = "1"
            process = subprocess.run(args, env=env, capture_output=True, text=True, timeout=120)
            if process.returncode:
                raise AssertionError(f"engine failed: {process.returncode}\n{process.stdout[-2000:]}\n{process.stderr[-2000:]}")
            advertised = [t.get("name") for t in server.requests[0].get("tools", [])]
            assert "cpp_kdb" in advertised, advertised
            outputs = {}
            for request in server.requests:
                for item in request.get("input", []):
                    if item.get("type") == "function_call_output":
                        outputs[item["call_id"]] = json.dumps(item.get("output"), ensure_ascii=False)
            assert len(outputs) == len(steps), (outputs, process.stderr[-2000:])
            assert "cpp_kdb" in process.stdout + process.stderr, "tool call invisible"
            logs = Path(env["BYTEASK_TOOL_LOG"]).read_text() if Path(env["BYTEASK_TOOL_LOG"]).exists() else ""
            return outputs, process.stdout, process.stderr, logs
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def tui_scenario(engine, connect, output_dir):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Responses)
    server.steps = [connect, {"action": "inspect", "session": 1, "command": "summary"},
                    {"action": "disconnect", "session": 1}]
    server.requests = []
    server.side_requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
    transcript = bytearray()
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix="byteask-kdb-tui-") as scratch:
            args = provider_args(engine, server.server_port) + [
                "-c", f'projects={{ {json.dumps(scratch)} = {{ trust_level="trusted" }} }}',
                "-c", 'check_for_update_on_startup=false',
                "-c", 'animations=false',
                "-c", 'tui.notifications=false', "-a", "never", "--no-alt-screen",
                "--ax-screen-reader", "-m", "gpt-5.4", "-C", scratch,
                "Inspect the prepared kernel using the fixture sequence."]
            env = dict(os.environ, TERM="xterm-256color", BYTEASK_ALLOW_NATIVE_TOOLS="1",
                       BYTEASK_TOOL_LOG=str(Path(scratch) / "tools.jsonl"))
            process = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                                       env=env, start_new_session=True)
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline and process.poll() is None:
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    data = os.read(master, 65536)
                    transcript.extend(data)
                    if b"\x1b[6n" in data:
                        os.write(master, b"\x1b[1;1R")
                if b"KDB fixture completed." in transcript and len(server.requests) == 4:
                    break
            raw_text = transcript.decode(errors="replace")
            (output_dir / "tui.raw.txt").write_text(raw_text)
            text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", raw_text)
            (output_dir / "tui.txt").write_text(text)
            assert len(server.requests) == 4, text[-4000:]
            assert "Called cpp_kdb" in text, text[-4000:]
            summary_outputs = [item for item in server.requests[2].get("input", [])
                               if item.get("call_id") == "kdb-1"]
            assert "Linux" in json.dumps(summary_outputs), summary_outputs
            for request in server.side_requests:
                assert not any(tool.get("name") == "cpp_kdb" for tool in request.get("tools", []))
            os.write(master, b"/quit\r")
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                # Functional assertions are complete. Closing the fixture's
                # own UI process must not depend on terminal key timing.
                process.terminate()
                process.wait(timeout=10)
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        os.close(master)
        os.close(slave)
        server.shutdown()
        server.server_close()
        thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", type=Path, default=Path.home() / ".local/bin/byteask-engine")
    parser.add_argument("--port", type=int, default=2012)
    parser.add_argument("--vmlinux", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path("/tmp/byteask-kdb-e2e"))
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    connect = {"action": "connect", "transport": {"kind": "tcp", "host": "127.0.0.1", "port": args.port}, "timeout_secs": 10}
    if args.vmlinux:
        connect["vmlinux"] = str(args.vmlinux.resolve())
    steps = [connect,
             {"action": "inspect", "session": 1, "command": "summary"},
             {"action": "inspect", "session": 1, "command": "processes", "limit": 2},
             {"action": "inspect", "session": 1, "command": "dmesg", "limit": 5},
             {"action": "inspect", "session": 1, "command": "modules"},
             {"action": "inspect", "session": 1, "command": "summary\nreboot"},
             {"action": "inspect", "session": 1, "command": "summary", "approved": True},
             {"action": "inspect", "session": 1, "command": "summary", "limit": 0},
             {"action": "status", "session": 1},
             {"action": "list"},
             {"action": "disconnect", "session": 1},
             {"action": "inspect", "session": 1, "command": "summary"},
             connect,
             {"action": "disconnect", "session": 2},
             {"action": "list"}]
    with tempfile.TemporaryDirectory(prefix="byteask-kdb-symbols-") as symbols_dir:
        if args.vmlinux:
            quoted_symbols = Path(symbols_dir) / 'matching kernel "quoted".elf'
            quoted_symbols.symlink_to(args.vmlinux.resolve())
            steps[12] = dict(connect, vmlinux=str(quoted_symbols))
        outputs, stdout, stderr, logs = scenario(args.engine, steps)
    for index in [0, 1, 8, 9, 12]:
        assert "sysname" in outputs[f"kdb-{index}"] and "Linux" in outputs[f"kdb-{index}"], outputs
    assert "truncated to line limit" in outputs["kdb-2"]
    assert "KGDB" in outputs["kdb-3"], outputs["kdb-3"]
    for index in [5, 6, 7]:
        assert "🔧" not in outputs[f"kdb-{index}"], outputs[f"kdb-{index}"]
    assert "No kernel session" in outputs["kdb-11"]
    assert "No open kernel" in outputs["kdb-14"]
    if args.vmlinux:
        assert "matching kernel" in outputs["kdb-12"]
    assert any(json.loads(line)["tool"] == "cpp_kdb" for line in logs.splitlines())
    (args.output_dir / "success.json").write_text(json.dumps(outputs, indent=2, ensure_ascii=False))
    (args.output_dir / "exec.jsonl").write_text(stdout)
    (args.output_dir / "exec.stderr").write_text(stderr)
    (args.output_dir / "tool-calls.jsonl").write_text(logs)
    monitor_only = dict(connect)
    monitor_only.pop("vmlinux", None)
    outputs, _, _, _ = scenario(args.engine, [monitor_only,
        {"action": "inspect", "session": 1, "command": "backtrace"},
        {"action": "inspect", "session": 1, "command": "registers"},
        {"action": "disconnect", "session": 1}])
    assert "not supplied" in outputs["kdb-0"], outputs
    assert "🔧" in outputs["kdb-1"] and "🔧" in outputs["kdb-2"], outputs
    (args.output_dir / "monitor-only.json").write_text(json.dumps(outputs, indent=2, ensure_ascii=False))
    outputs, _, _, _ = scenario(args.engine, [connect, {"action": "list"}], trusted=False)
    assert "BYTEASK_ALLOW_NATIVE_TOOLS" in outputs["kdb-0"]
    assert "No open kernel" in outputs["kdb-1"]
    edge_steps = [
        {"action": "connect", "transport": {"kind": "tcp", "host": "|touch /tmp/kdb-injected", "port": 1}},
        {"action": "connect", "transport": {"kind": "serial", "device": "/dev/no-such-kdb-device"}},
        {"action": "connect", "transport": {"kind": "tcp", "host": "127.0.0.1", "port": 1}, "timeout_secs": 1},
        {"action": "disconnect", "session": 999},
        {"action": "list"},
    ]
    outputs, stdout, stderr, _ = scenario(args.engine, edge_steps, json_output=False)
    assert "pipe transports" in outputs["kdb-0"]
    assert "No such file" in outputs["kdb-1"]
    assert "Connection closed" in outputs["kdb-2"]
    assert "No kernel session" in outputs["kdb-3"]
    assert "No open kernel" in outputs["kdb-4"]
    (args.output_dir / "edges.json").write_text(json.dumps(outputs, indent=2, ensure_ascii=False))
    (args.output_dir / "human-exec.txt").write_text(stdout + stderr)
    # Exercise the serial-device path against the same real guest through a
    # host PTY. The relay owns only this local connection and its descriptors.
    master, slave = pty.openpty()
    serial_socket = socket.create_connection(("127.0.0.1", args.port), timeout=10)
    relay_stop = threading.Event()

    def relay():
        while not relay_stop.is_set():
            try:
                ready, _, _ = select.select([master, serial_socket], [], [], 0.1)
                for source in ready:
                    if source == master:
                        data = os.read(master, 4096)
                        if not data:
                            return
                        serial_socket.sendall(data)
                    else:
                        data = serial_socket.recv(4096)
                        if not data:
                            return
                        os.write(master, data)
            except OSError:
                return

    relay_thread = threading.Thread(target=relay, daemon=True)
    relay_thread.start()
    try:
        serial_steps = [{"action": "connect", "transport": {
            "kind": "serial", "device": os.ttyname(slave), "baud": 115200}},
            {"action": "inspect", "session": 1, "command": "summary"},
            {"action": "disconnect", "session": 1}]
        outputs, _, _, _ = scenario(args.engine, serial_steps)
        assert "Linux" in outputs["kdb-1"], outputs
        (args.output_dir / "serial.json").write_text(json.dumps(outputs, indent=2, ensure_ascii=False))
    finally:
        relay_stop.set()
        relay_thread.join(timeout=2)
        serial_socket.close()
        os.close(master)
        os.close(slave)
    tui_scenario(args.engine, connect, args.output_dir)
    print(f"PASS: installed engine; 32 scripted calls; live TCP/serial KGDB/KDB inspection/reconnect with and without symbols, trust gate, validation, failures, JSON/human/TUI visibility. Evidence: {args.output_dir}")


if __name__ == "__main__":
    main()
