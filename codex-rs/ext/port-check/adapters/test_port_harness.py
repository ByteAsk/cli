"""Tests for port_harness.py (stdlib unittest): python3 -m unittest test_port_harness -v"""

import io
import unittest

import port_harness as ph


class Recorder(ph.Strategy):
    def __init__(self):
        self.seen = []

    def on_init(self, ctx, ev):
        self.seen.append(("init", ctx.seed, ctx.symbols, ctx.params, ctx.depth, ctx.now))

    def on_md(self, ctx, ev):
        self.seen.append(("md", ev.sym, ev.kind, ev.rec, ev.bids, ev.asks, ev.px))
        ctx.new_order("o1", ev.sym, "B", ev.asks[0][0], 2, tif="ioc")
        ctx.decide("flag", True)
        ctx.decide("edge", 0.1)
        ctx.decide("regime", "calm")
        ctx.diag("fair", float("nan"))
        ctx.diag("hi", float("-inf"))
        ctx.diag("x", 3)


def run_lines(strategy, lines):
    out = io.StringIO()
    ph.run(strategy, stdin=io.StringIO("".join(l + "\n" for l in lines)), stdout=out)
    return out.getvalue().splitlines()


class AdapterTest(unittest.TestCase):
    def test_events_parse_and_answers_use_the_protocol_spellings(self):
        s = Recorder()
        out = run_lines(s, [
            "init step=0 ts=5 seed=42 syms=A,B depth=2 p.k=v",
            "md step=1 ts=6 rec=3 sym=A kind=add id=9 side=B px=100 qty=1 bp1=100 bq1=1 ap1=101 aq1=4 ap2=102 aq2=1",
        ])
        self.assertEqual(s.seen[0], ("init", 42, ["A", "B"], {"k": "v"}, 2, 5))
        self.assertEqual(s.seen[1], ("md", "A", "add", 3, [(100, 1)], [(101, 4), (102, 1)], 100))
        self.assertEqual(out, [
            "done",
            "new id=o1 sym=A side=B px=101 qty=2 tif=ioc",
            "decide flag=1",
            "decide edge=0.1",
            "decide regime=calm",
            "diag fair=nan",
            "diag hi=-inf",
            "diag x=3.0",
            "done",
        ])

    def test_float_prices_bools_and_bad_tokens_are_refused(self):
        ctx = ph.Context()
        with self.assertRaises(TypeError):
            ctx.new_order("o", "A", "B", 100.5, 1)
        with self.assertRaises(TypeError):
            ctx.new_order("o", "A", "B", 100, True)
        with self.assertRaises(ValueError):
            ctx.new_order("o 1", "A", "B", 100, 1)
        with self.assertRaises(ValueError):
            ctx.new_order("o", "A", "buy", 100, 1)
        with self.assertRaises(ValueError):
            ctx.new_order("o", "A", "B", 100, 1, tif="gtc")
        with self.assertRaises(TypeError):
            ctx.diag("x", "1.0")
        with self.assertRaises(ValueError):
            ctx.decide("x", "two words")

    def test_empty_symbol_list_and_unknown_events(self):
        s = Recorder()
        run_lines(s, ["init step=0 ts=1 seed=0 syms= depth=1"])
        self.assertEqual(s.seen[0][2], [])
        with self.assertRaises(ValueError):
            run_lines(ph.Strategy(), ["quote step=1 ts=1"])


if __name__ == "__main__":
    unittest.main()
