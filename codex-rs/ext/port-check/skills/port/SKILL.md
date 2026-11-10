---
name: port
description: "Use when asked to port, convert, rewrite or speed up a Python trading strategy (or any Python component that turns market data into orders) as C++. Drives a checked port: freeze the Python behaviour, port one component at a time, prove each step with port_check replay, and finish with a proof report. Do not use for general C++ work, for Python-to-C++ ports of code that does not consume events and emit decisions, or to review an existing port without running it."
metadata:
  short-description: "Checked Python-to-C++ port of a trading strategy, proven by replay"
---

# Checked port: Python strategy to C++

A port is correct when the C++ version sends the same orders and makes the same decisions as
the Python version on recorded market data, under one deterministic harness. That is what
`port_check` checks. Reading the two programs side by side is not evidence; whole-program
translation without a check fails far more often than it works. Keep the Python version as the
oracle for the whole job.

## 1. Freeze the Python behaviour

1. Record exactly what is being ported: the Python commit (or a copy of the files) and the
   strategy's parameters. Never change the Python logic during the port; if it has a bug, port
   the bug and report it separately.
2. Put the strategy behind the adapter: `byteask-port-check adapters <dir>` writes
   `port_harness.py` and `port_harness.hpp`. Wrap the strategy in a `port_harness.Strategy`
   subclass whose callbacks call the existing code. Time must come from `ctx.now`, randomness
   from `ctx.seed`, prices and quantities must be integers (ticks or fixed point).
3. Get a recording the harness can replay: JSON Lines of `add/cancel/delete/exec/replace/trade`
   events (an exchange capture goes through a decoder first; Nasdaq ITCH 5.0 has one:
   `byteask-itch decode`). Prefer real sessions that make the strategy trade, reject and cancel.
4. Freeze check: run `port_check` with the Python strategy in BOTH the `python` and `cpp` slots.
   It must PASS. NOT RUN (not reproducible) means the Python version itself varies between runs
   (wall clock, unseeded randomness, set or dict iteration that changes with `PYTHONHASHSEED`,
   threads, files that change): fix that first, because no port can match a moving target.
   NO CLAIM means the strategy never traded on that recording: find one where it does.

## 2. Port one component at a time

List the components (signal maths, order logic, position and risk bookkeeping, parameter
handling) and port the pure ones first. For each component:

- Expose its intermediate values in BOTH versions with `ctx.decide(...)` (exact) or
  `ctx.diag(...)` (numbers that may be compared within a tolerance), so the first difference
  points at the component, not just at the order it eventually changed.
- When the component can be driven on its own, give it a small driver in each language that
  reads recorded inputs as JSON Lines and prints outputs as JSON Lines, and compare them with
  `byteask-replay run <spec.json>` before wiring it into the strategy.
- Watch for the differences that break most ports:
  - Python `//` and `%` round toward minus infinity; C++ `/` and `%` truncate toward zero. They
    differ for negative numbers (prices of spreads, signed positions).
  - Python ints never overflow; C++ `int64_t` does. Check products like `px * qty`.
  - Python `int / int` is the exactly-rounded quotient; C++ `double(a) / double(b)` rounds the
    operands first. They differ once an operand is above 2^53.
  - `round()` in Python rounds half to even; `std::round` rounds half away from zero.
  - Floating-point results depend on operation order: keep the same association, the same
    accumulation order, and build with `-ffp-contract=off` (no fused multiply-add) unless the
    Python side is shown to match. Do not use `-ffast-math`.
  - Python dicts iterate in insertion order; `std::unordered_map` does not. Never let the
    iteration order of a hash container decide an order or its sequence.
  - Port the random generator itself (the exact algorithm and seed handling); `std::`
    distributions are implementation-defined and will not match `random`.
  - String formatting of floats differs; that is fine for `diag` (compared as numbers) but a
    float must never be part of an order id or a decision token.

## 3. Check

Build the C++ version and run `port_check` (the tool, or `byteask-port-check check req.json`):

- `python`: the frozen Python strategy; `cpp`: the C++ binary; `recording`: the replay file.
- Exercise the paths that matter: `harness.reject_every` or `reject_ppm` (reject handling),
  a nonzero `harness.latency_ns`, several `harness.seed` values, more than one recording.
- Run it again on a sanitizer build (`-fsanitize=address,undefined`); a sanitizer abort shows
  up as a FAIL at the step it happened, with the report in the stderr tail.
- Orders and decisions are always compared exactly. Use `diag_tolerance` only for diagnostic
  numbers, and say why in the report (for example "1 ulp: libm exp differs").

Read a FAIL from its `first_difference`: the step, the event that step answered (`in`), the
field and both values. Fix the C++ (never the harness, the rules or the Python), rebuild, rerun.
A C++ that differs from itself between its two runs ("not reproducible") reads uninitialised
memory, orders something by pointer value, or races; fix that before anything else. NOT RUN and
NO CLAIM are not passes; never report them as one.

## 4. Proof report

Finish with `PORT-REPORT.md` next to the C++ code:

- what was ported: Python commit (and the entry script sha256 from the result), C++ commit and
  the executable sha256 from the result;
- every `port_check` run: status, recording file and sha256, records, harness settings, any
  diag tolerance and its reason, and the coverage counts (orders, fills, rejects, cancels,
  timers, decisions);
- what was not exercised (the result lists it) and what is out of scope: speed and allocation
  results come from the perf lab, which `port_check` does not replace;
- known Python bugs that were ported on purpose.

Claim only what was checked: "matches on these recordings under these settings", never
"equivalent".
