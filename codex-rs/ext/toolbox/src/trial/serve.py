"""ByteAsk context engine query server (written by `byteask company trial`).

Runs `bctx serve` from this install with this install's pinned clang and parsers; started by
ByteAsk's context tools as `python3 -I serve.py --data <index data> serve`.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "python"))
os.environ.setdefault("BCTX_LLVM_ROOT", os.path.join(HERE, "llvm"))
os.environ.setdefault("BCTX_FACTS_BIN", os.path.join(HERE, "bin", "bctx-facts"))
os.environ.setdefault("BCTX_TRACE_BIN", os.path.join(HERE, "bin", "bctx-trace"))
os.environ.setdefault("BCTX_SCIP_CLANG_BIN", os.path.join(HERE, "bin", "scip-clang"))

from bctx.cli import main  # noqa: E402

main()
