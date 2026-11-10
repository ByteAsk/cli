#!/usr/bin/env python3
"""vtreplay — replay a raw terminal typescript into the FINAL rendered screen.

The persistent settings screen repaints itself in place (cursor-up + clear-line),
so the raw byte stream contains every frame; only an interpreter can prove what
the user actually SAW at the end. This is deliberately small but must DRAIN any
CSI it doesn't understand (menu tests would otherwise pass on garbage).

Handles: \\r \\n \\b, CSI n A/B (up/down), CSI n C/D (right/left), CSI K (clear
to EOL), CSI 0J/J (clear below), CSI 2J (clear screen), CSI H / CSI r;cH (home/
move), CSI ?...h/l (private modes — recorded as flags), CSI ...m (SGR, dropped),
any OTHER CSI fully drained to its final byte and ignored, ESC + single char
ignored, OSC (ESC ] ... BEL/ST) drained.

Usage:  vtreplay.py <typescript-file>        -> prints the final screen
        vtreplay.py --flags <file>           -> prints one flag per line
        vtreplay.py --selftest               -> exercises itself on fixtures
Flags reported: mode_2004_on/off, mode_25_hide/show, cursor_up_used,
clear_eol_used, clear_below_used.
"""
import sys


class VT:
    def __init__(self, alt_buffer=False):
        self.alt_buffer = alt_buffer
        self.rows = {}          # row -> {col: ch}
        self.r = 0
        self.c = 0
        self.flags = set()

    def put(self, ch):
        self.rows.setdefault(self.r, {})[self.c] = ch
        self.c += 1

    def clear_eol(self):
        row = self.rows.get(self.r, {})
        for col in [k for k in row if k >= self.c]:
            del row[col]
        self.flags.add("clear_eol_used")

    def clear_below(self):
        row = self.rows.get(self.r, {})
        for col in [k for k in row if k >= self.c]:
            del row[col]
        for rr in [k for k in self.rows if k > self.r]:
            del self.rows[rr]
        self.flags.add("clear_below_used")

    def feed(self, data):
        # Decode the raw stream as UTF-8 first so a multi-byte glyph (e.g. the
        # middot ·, bytes C2 B7) lands in ONE cell — matching what the user sees
        # and the UTF-8 assertion strings the tests grep for. All control/CSI
        # syntax is ASCII (code points <= 0x7E), and UTF-8 glyphs decode to code
        # points >= 0x80 that only ever reach the printable branch, so parsing on
        # code points is safe.
        if isinstance(data, (bytes, bytearray)):
            data = data.decode("utf-8", "replace")
        i, n = 0, len(data)
        while i < n:
            b = ord(data[i])
            if b == 0x1B:                              # ESC
                i += 1
                if i >= n:
                    break
                nxt = ord(data[i])
                if nxt == ord("["):                    # CSI
                    i += 1
                    params = []
                    while i < n and not (0x40 <= ord(data[i]) <= 0x7E):
                        params.append(data[i])
                        i += 1
                    if i >= n:
                        break
                    final = data[i]
                    i += 1
                    self.csi("".join(params), final)
                elif nxt == ord("]"):                  # OSC: drain to BEL or ESC\
                    i += 1
                    while i < n and ord(data[i]) != 0x07:
                        if ord(data[i]) == 0x1B and i + 1 < n and data[i + 1] == "\\":
                            i += 1
                            break
                        i += 1
                    i += 1
                else:                                  # ESC + one char (e.g. ESC O A)
                    i += 1
                    if nxt == ord("O") and i < n:
                        i += 1                         # SS3 arrows consume one more
                continue
            if b == 0x0D:                              # \r
                self.c = 0
            elif b == 0x0A:                            # \n
                self.r += 1
                self.c = 0
            elif b == 0x08:                            # \b
                self.c = max(0, self.c - 1)
            elif b == 0x07:                            # BEL
                pass
            elif b >= 0x20:
                self.put(data[i])
            i += 1

    def csi(self, params, final):
        def num(default=1):
            try:
                return int(params or default)
            except ValueError:
                return default
        if final == "A":
            self.r = max(0, self.r - num())
            self.flags.add("cursor_up_used")
        elif final == "B":
            self.r += num()
        elif final == "C":
            self.c += num()
        elif final == "D":
            self.c = max(0, self.c - num())
        elif final == "K":
            self.clear_eol()
        elif final == "J":
            if params in ("", "0"):
                self.clear_below()
            elif params == "2":
                self.rows = {}
                self.flags.add("clear_below_used")
        elif final == "H":
            if ";" in params:
                pr, pc = (params.split(";") + ["1"])[:2]
                try:
                    self.r, self.c = max(0, int(pr) - 1), max(0, int(pc) - 1)
                except ValueError:
                    self.r = self.c = 0
            else:
                self.r = self.c = 0
        elif final in ("h", "l") and params.startswith("?"):
            mode = params[1:]
            if mode == "2004":
                self.flags.add("mode_2004_on" if final == "h" else "mode_2004_off")
            elif mode == "25":
                self.flags.add("mode_25_hide" if final == "l" else "mode_25_show")
            elif mode == "1049":
                # A real terminal's alternate buffer starts blank, and leaving it
                # restores the primary. Opt-in (--alt-buffer) so the wrapper's menu
                # tests, which never switch buffers, keep their existing semantics.
                self.flags.add("mode_1049_on" if final == "h" else "mode_1049_off")
                if self.alt_buffer:
                    self.rows = {}
                    self.r = self.c = 0
        # SGR (m) and every other CSI: parsed above, deliberately ignored.

    def screen(self):
        if not self.rows:
            return ""
        out = []
        for r in range(0, max(self.rows) + 1):
            row = self.rows.get(r, {})
            line = "".join(row.get(c, " ") for c in range(0, (max(row) + 1) if row else 0))
            out.append(line.rstrip())
        # trim trailing blank lines
        while out and not out[-1]:
            out.pop()
        return "\n".join(out)


def replay(path, alt_buffer=False):
    with open(path, "rb") as f:
        data = f.read()
    vt = VT(alt_buffer=alt_buffer)
    vt.feed(data)
    return vt


def selftest():
    ok = True

    def check(name, cond):
        nonlocal ok
        print(("  ok  " if cond else "FAIL  ") + name)
        ok = ok and cond

    # 1. in-place repaint: 3 frames of a 3-line region -> final frame only
    frames = b""
    for word in (b"one", b"two", b"three"):
        frames += b"\r\x1b[Kheader\n\r\x1b[Kitem: " + word + b"\n\r\x1b[Kfooter\n"
        frames += b"\x1b[3A"
    frames += b"\x1b[3B"                       # settle below like a finalize
    vt = VT(); vt.feed(frames)
    s = vt.screen()
    check("repaint keeps ONE region", s.count("header") == 1)
    check("last frame wins", "item: three" in s and "two" not in s)
    # 2. shorter frame + clear-below removes orphans
    vt = VT()
    vt.feed(b"line1\nline2\nline3\n\x1b[3Aline1'\n\x1b[0J")
    check("clear below removes orphan rows", "line3" not in vt.screen())
    # 3. unknown CSI fully drained (params never leak as text)
    vt = VT(); vt.feed(b"\x1b[1;5Ahello\x1b[38;2;1;2;3mworld\x1b[0m")
    check("unknown/SGR CSI drained", vt.screen().endswith("helloworld")
          or "world" in vt.screen())
    check("no CSI params leaked", "1;5" not in vt.screen() and "38;2" not in vt.screen())
    # 4. modes recorded
    vt = VT(); vt.feed(b"\x1b[?2004h\x1b[?25lhi\x1b[?2004l\x1b[?25h")
    check("bracketed-paste flags", {"mode_2004_on", "mode_2004_off"} <= vt.flags)
    check("cursor hide/show flags", {"mode_25_hide", "mode_25_show"} <= vt.flags)
    # 5. OSC drained, SS3 arrows consumed
    vt = VT(); vt.feed(b"\x1b]0;title\x07ok\x1bOAx")
    check("OSC drained", vt.screen().startswith("ok"))
    # 6. split CSI across feeds is tolerated at the byte level (single feed here,
    #    split within the stream): ESC[ then params then final in one stream
    vt = VT(); vt.feed(b"a\x1b[" + b"1" + b"Ab")
    check("split-in-stream CSI ok", "b" in vt.screen())
    print("selftest: " + ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "--selftest":
        return selftest()
    args = [a for a in sys.argv[1:] if a != "--alt-buffer"]
    alt_buffer = "--alt-buffer" in sys.argv[1:]
    flags_only = bool(args) and args[0] == "--flags"
    path = (args[1] if len(args) > 1 else None) if flags_only else (args[0] if args else None)
    if not path:
        sys.stderr.write(__doc__)
        return 2
    vt = replay(path, alt_buffer=alt_buffer)
    if flags_only:
        for f in sorted(vt.flags):
            print(f)
    else:
        print(vt.screen())
    return 0


if __name__ == "__main__":
    sys.exit(main())
