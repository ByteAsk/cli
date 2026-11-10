"""Test strategy for port_check: trades on top-of-book imbalance, quotes passively, uses a
seeded generator, timers, decisions and diagnostics, so every part of the protocol is used.
imbalance.cpp is its port. Variants for the tests are chosen with the VARIANT parameter."""

import time

from port_harness import Strategy, run

MASK = (1 << 64) - 1


class Imbalance(Strategy):
    def on_init(self, ctx, ev):
        self.variant = ctx.params.get("variant", "")
        self.state = ctx.seed or 88172645463325252
        self.n = 0
        self.orders = {}  # id -> (sym, side); insertion order is deterministic
        self.quotes = []  # resting quote ids, oldest first
        self.position = {s: 0 for s in ctx.symbols}
        self.max_pos = int(ctx.params.get("max_pos", "30"))
        self.every = int(ctx.params.get("quote_every", "7"))
        self.timer_ns = int(ctx.params.get("timer_ns", "400000"))
        self.beats = 0
        self.md = 0
        ctx.set_timer(ctx.now + self.timer_ns, "hb")
        if self.variant == "hash_order":
            names = set("sym%02d" % i for i in range(24))
            ctx.decide("first", next(iter(names)))

    def rand(self):
        x = self.state
        x ^= (x << 13) & MASK
        x ^= x >> 7
        x ^= (x << 17) & MASK
        self.state = x
        return x

    def send(self, ctx, sym, side, px, qty, tif):
        self.n += 1
        oid = "o%d" % self.n
        self.orders[oid] = (sym, side)
        ctx.new_order(oid, sym, side, px, qty, tif=tif)
        return oid

    def on_md(self, ctx, ev):
        self.md += 1
        if self.variant == "wall_clock":
            ctx.decide("t", time.time_ns() % 1000003)
        if not ev.bids or not ev.asks:
            return
        (bp, bq), (ap, aq) = ev.bids[0], ev.asks[0]
        ctx.diag("fair", (bp * aq + ap * bq) / (bq + aq))
        ctx.diag("spread", float(ap - bp))
        imb = bq - aq
        ctx.decide("imb", 1 if imb > 0 else (-1 if imb < 0 else 0))
        if self.variant == "no_orders":
            return
        qty = 1 + self.rand() % 3
        pos = self.position.get(ev.sym, 0)
        if bq > 2 * aq and pos + qty <= self.max_pos:
            self.send(ctx, ev.sym, "B", ap, qty, "ioc")
        elif aq > 2 * bq and pos - qty >= -self.max_pos:
            self.send(ctx, ev.sym, "S", bp, qty, "ioc")
        elif self.md % self.every == 0:
            for oid in self.quotes:
                ctx.cancel(oid)
            self.quotes = []
            side = "B" if self.rand() % 2 == 0 else "S"
            px = bp if side == "B" else ap
            self.quotes.append(self.send(ctx, ev.sym, side, px, 2, "day"))

    def on_fill(self, ctx, ev):
        sym, side = self.orders[ev.id]
        self.position[sym] += ev.qty if side == "B" else -ev.qty
        ctx.decide("pos", self.position[sym])
        if ev.leaves == 0 and ev.id in self.quotes:
            self.quotes.remove(ev.id)

    def on_reject(self, ctx, ev):
        ctx.decide("rej", ev.reason)
        if ev.id in self.quotes:
            self.quotes.remove(ev.id)

    def on_cancelled(self, ctx, ev):
        ctx.decide("cxl", ev.qty)

    def on_timer(self, ctx, ev):
        self.beats += 1
        ctx.decide("beat", self.beats)
        ctx.set_timer(ctx.now + self.timer_ns, "hb")

    def on_end(self, ctx, ev):
        ctx.decide("final", sum(self.position.values()))


if __name__ == "__main__":
    run(Imbalance())
