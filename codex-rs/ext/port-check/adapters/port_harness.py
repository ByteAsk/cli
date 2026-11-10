"""ByteAsk port_check adapter for Python strategies (harness protocol 1).

Wrap a strategy so the port_check harness can drive it, then port the same class to C++ with
port_harness.hpp. Both adapters speak the same line protocol, so the harness can replay one
recording through both versions and compare every order and decision.

    from port_harness import Strategy, run

    class Imbalance(Strategy):
        def on_md(self, ctx, ev):
            if ev.bids and ev.asks and ev.bids[0][1] > 2 * ev.asks[0][1]:
                ctx.new_order("o1", ev.sym, "B", ev.asks[0][0], 1, tif="ioc")

    if __name__ == "__main__":
        run(Imbalance())

What the harness relies on (port_check checks it by running this version twice):
- Time is ctx.now (integer nanoseconds of market time). Never read the wall clock.
- Randomness starts from ctx.seed. Never use an unseeded random source.
- Prices and quantities are integers (ticks or fixed-point units). A float price is refused
  here, before it can turn into a silent rounding difference between the two versions.
- Do not iterate a set (or a dict keyed by objects with default hashing) where the order
  changes what you do: the two Python runs use different PYTHONHASHSEED values on purpose.
- stdout is the protocol channel: print nothing to it yourself. Log to stderr.

Decision and diagnostic values: ctx.decide(name, value) takes an int, a float or a token string
and is compared EXACTLY. ctx.diag(name, value) takes a number and is the only thing a tolerance
may apply to. A bool decision is sent as 1 or 0 (as in C++).
"""

import math
import re
import sys

PROTOCOL_VERSION = 1

_TOKEN = re.compile(r"[A-Za-z0-9._:+-]{1,64}\Z")


def _token(value, what):
    if not isinstance(value, str) or not _TOKEN.match(value):
        raise ValueError(
            "%s must be 1-64 characters from A-Z a-z 0-9 . _ : + - (got %r)" % (what, value)
        )
    return value


def _int(value, what):
    if type(value) is not int:
        raise TypeError(
            "%s must be an int (prices and quantities are integer units, never floats); got %s %r"
            % (what, type(value).__name__, value)
        )
    return value


def _number(value, what):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TypeError("%s must be a number; got %s %r" % (what, type(value).__name__, value))
    f = float(value)
    if math.isnan(f):
        return "nan"
    if math.isinf(f):
        return "inf" if f > 0 else "-inf"
    return repr(f)


class Event:
    """One event from the harness. Fields are strings; use the properties or int_()."""

    __slots__ = ("type", "step", "ts", "fields", "bids", "asks")

    def __init__(self, line):
        parts = line.split(" ")
        self.type = parts[0]
        self.fields = {}
        for part in parts[1:]:
            key, sep, value = part.partition("=")
            if not sep:
                raise ValueError("bad field %r in %r" % (part, line))
            self.fields[key] = value
        self.step = int(self.fields["step"])
        self.ts = int(self.fields["ts"])
        self.bids = self._levels("b")
        self.asks = self._levels("a")

    def _levels(self, side):
        out = []
        i = 1
        while ("%sp%d" % (side, i)) in self.fields:
            out.append((int(self.fields["%sp%d" % (side, i)]), int(self.fields["%sq%d" % (side, i)])))
            i += 1
        return out

    def get(self, key, default=None):
        return self.fields.get(key, default)

    def int_(self, key, default=None):
        v = self.fields.get(key)
        return default if v is None else int(v)

    sym = property(lambda self: self.fields.get("sym"))
    kind = property(lambda self: self.fields.get("kind"))
    side = property(lambda self: self.fields.get("side"))
    id = property(lambda self: self.fields.get("id"))
    reason = property(lambda self: self.fields.get("reason"))
    tag = property(lambda self: self.fields.get("tag"))
    liq = property(lambda self: self.fields.get("liq"))
    px = property(lambda self: self.int_("px"))
    qty = property(lambda self: self.int_("qty"))
    leaves = property(lambda self: self.int_("leaves"))
    new_id = property(lambda self: self.int_("new_id"))
    rec = property(lambda self: self.int_("rec"))


class Context:
    """What a strategy may read and do while answering one event."""

    def __init__(self):
        self.now = 0
        self.step = 0
        self.seed = 0
        self.symbols = []
        self.params = {}
        self.depth = 1
        self._out = []

    def new_order(self, id, sym, side, px, qty, tif="day"):
        if side not in ("B", "S"):
            raise ValueError("side must be 'B' or 'S' (got %r)" % (side,))
        if tif not in ("day", "ioc"):
            raise ValueError("tif must be 'day' or 'ioc' (got %r)" % (tif,))
        self._out.append(
            "new id=%s sym=%s side=%s px=%d qty=%d tif=%s"
            % (_token(id, "order id"), _token(sym, "symbol"), side, _int(px, "px"), _int(qty, "qty"), tif)
        )

    def cancel(self, id):
        self._out.append("cancel id=%s" % _token(id, "order id"))

    def set_timer(self, at, tag):
        self._out.append("timer at=%d tag=%s" % (_int(at, "timer time"), _token(tag, "timer tag")))

    def decide(self, name, value):
        _token(name, "decision name")
        if isinstance(value, bool):
            text = "1" if value else "0"
        elif isinstance(value, int):
            text = str(value)
        elif isinstance(value, float):
            text = _number(value, "decision")
        else:
            text = _token(value, "decision value")
        self._out.append("decide %s=%s" % (name, text))

    def diag(self, name, value):
        self._out.append("diag %s=%s" % (_token(name, "diag name"), _number(value, "diag value")))


class Strategy:
    """Override the callbacks you need. Each receives (ctx, event)."""

    def on_init(self, ctx, ev):
        pass

    def on_md(self, ctx, ev):
        pass

    def on_ack(self, ctx, ev):
        pass

    def on_fill(self, ctx, ev):
        pass

    def on_reject(self, ctx, ev):
        pass

    def on_cancelled(self, ctx, ev):
        pass

    def on_cancel_reject(self, ctx, ev):
        pass

    def on_timer(self, ctx, ev):
        pass

    def on_end(self, ctx, ev):
        pass


_HANDLERS = {
    "init": "on_init",
    "md": "on_md",
    "ack": "on_ack",
    "fill": "on_fill",
    "reject": "on_reject",
    "cancelled": "on_cancelled",
    "cancel_reject": "on_cancel_reject",
    "timer": "on_timer",
    "end": "on_end",
}


def run(strategy, stdin=None, stdout=None):
    """Answer harness events until the input ends. Returns the process exit code (0)."""
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    ctx = Context()
    for raw in stdin:
        line = raw.rstrip("\r\n")
        if not line:
            continue
        ev = Event(line)
        ctx.now = ev.ts
        ctx.step = ev.step
        if ev.type == "init":
            ctx.seed = int(ev.fields["seed"])
            syms = ev.fields.get("syms", "")
            ctx.symbols = syms.split(",") if syms else []
            ctx.depth = int(ev.fields.get("depth", "1"))
            ctx.params = {k[2:]: v for k, v in ev.fields.items() if k.startswith("p.")}
        handler = _HANDLERS.get(ev.type)
        if handler is None:
            raise ValueError("unknown event type %r (adapter is protocol %d)" % (ev.type, PROTOCOL_VERSION))
        ctx._out = []
        getattr(strategy, handler)(ctx, ev)
        ctx._out.append("done")
        stdout.write("\n".join(ctx._out) + "\n")
        stdout.flush()
    return 0
