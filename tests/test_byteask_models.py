#!/usr/bin/env python3
"""byteask_models.py — registry + catalog-merge (the boot-critical path).

The catalog REPLACES the engine's bundled catalog, so a corrupt or base-dropping
merge bricks engine startup. These tests lock the mandatory regressions:
  * merge NEVER drops a base cloud model
  * a corrupt/empty base catalog leaves the file UNCHANGED (keep-last-good)
  * the slim template strips the Codex/GPT identity + sets function apply_patch
  * registry writes are atomic + 0600 + idempotent
"""
import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers")
sys.path.insert(0, HERE)
import byteask_models as M

_BASE_CATALOG = {"models": [
    {"slug": "gpt-5.4", "display_name": "GPT-5.4", "base_instructions": "You are Codex, a coding agent based on GPT-5.",
     "model_messages": {"instructions_template": "You are Codex..."}, "apply_patch_tool_type": "freeform",
     "context_window": 272000, "max_context_window": 1000000, "supports_reasoning_summaries": True,
     "supported_reasoning_levels": [{"effort": "low"}], "support_verbosity": True, "prefer_websockets": True,
     "service_tiers": [{"id": "priority"}], "additional_speed_tiers": ["fast"], "supports_search_tool": True,
     "include_skills_usage_instructions": False, "visibility": "list", "priority": 0},
    {"slug": "claude-opus-4-8", "display_name": "Claude Opus 4.8", "priority": 11},
    {"slug": "gemini-3.5-flash", "display_name": "Gemini 3.5 Flash", "priority": 20},
]}


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="byteask-models-")
        self.cfg = os.path.join(self.tmp, "byok-config.json")
        self.cat = os.path.join(self.tmp, "models-catalog.json")
        with open(self.cat, "w") as f:
            json.dump(copy.deepcopy(_BASE_CATALOG), f)

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def add(self, alias, ep):
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"), "add", self.cfg, alias],
                           input=json.dumps(ep).encode(), capture_output=True)
        self.assertEqual(p.returncode, 0, p.stderr.decode())

    def merge(self):
        return M.cmd_merge_catalog(self.cfg, self.cat)

    def catalog(self):
        with open(self.cat) as f:
            return json.load(f)


class TestRegistry(Base):
    def test_add_get_remove_roundtrip(self):
        self.add("dgx", {"base_url": "http://dgx:8000", "model_id": "Q", "wire": "chat"})
        self.assertEqual(M.cmd_get, M.cmd_get)   # sanity
        cfg = json.load(open(self.cfg))
        self.assertEqual(cfg["endpoints"]["dgx"]["model_id"], "Q")
        self.assertEqual(oct(os.stat(self.cfg).st_mode)[-3:], "600")   # 0600
        M.cmd_remove(self.cfg, "dgx")
        self.assertEqual(json.load(open(self.cfg)).get("endpoints"), {})

    def test_add_preserves_other_keys(self):
        with open(self.cfg, "w") as f:
            json.dump({"keys": {"openai": "sk"}, "jwt": "j", "local_token": "t"}, f)
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        cfg = json.load(open(self.cfg))
        self.assertEqual(cfg["keys"]["openai"], "sk")     # BYOK keys untouched
        self.assertEqual(cfg["jwt"], "j")
        self.assertIn("dgx", cfg["endpoints"])


class TestMergeSlimTemplate(Base):
    def test_slim_template_strips_codex_identity(self):
        self.add("dgx", {"base_url": "http://dgx:8000", "model_id": "Qwen", "wire": "chat",
                         "context_window": 32768, "display_name": "Qwen DGX"})
        self.assertEqual(self.merge(), 0)
        e = next(m for m in self.catalog()["models"] if m["slug"] == "self/dgx")
        self.assertNotIn("Codex", e["base_instructions"])
        self.assertNotIn("GPT-5", e["base_instructions"])
        self.assertNotIn("Codex", e["model_messages"]["instructions_template"])
        # apply_patch_tool_type is a single-variant engine enum (`freeform` only) —
        # any other value bricks engine startup, so the slim template keeps the base's.
        self.assertEqual(e["apply_patch_tool_type"], "freeform")
        self.assertEqual(e["context_window"], 32768)
        self.assertEqual(e["display_name"], "Qwen DGX")
        self.assertFalse(e["supports_search_tool"])
        self.assertFalse(e["supports_reasoning_summaries"])
        self.assertEqual(e["supported_reasoning_levels"], [])

    def test_merge_is_idempotent(self):
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        self.merge(); self.merge(); self.merge()
        slugs = [m["slug"] for m in self.catalog()["models"]]
        self.assertEqual(slugs.count("self/dgx"), 1)      # never duplicated


class TestNeverCorrupt(Base):
    def test_never_drops_a_base_model(self):
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        self.merge()
        slugs = {m["slug"] for m in self.catalog()["models"]}
        for base in ("gpt-5.4", "claude-opus-4-8", "gemini-3.5-flash"):
            self.assertIn(base, slugs)

    def test_corrupt_base_catalog_left_unchanged(self):
        with open(self.cat, "w") as f:
            f.write("{ this is not valid json ")
        before = open(self.cat).read()
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        rc = self.merge()
        self.assertEqual(rc, 1)                            # refused
        self.assertEqual(open(self.cat).read(), before)    # keep-last-good

    def test_empty_models_catalog_left_unchanged(self):
        with open(self.cat, "w") as f:
            json.dump({"models": []}, f)
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        self.assertEqual(self.merge(), 1)
        self.assertEqual(self.catalog()["models"], [])

    def test_no_endpoints_strips_stale_self_rows(self):
        cat = self.catalog()
        cat["models"].append({"slug": "self/old", "display_name": "stale"})
        with open(self.cat, "w") as f:
            json.dump(cat, f)
        self.assertEqual(self.merge(), 0)                  # no endpoints registered
        slugs = {m["slug"] for m in self.catalog()["models"]}
        self.assertNotIn("self/old", slugs)                # stale self row removed
        self.assertIn("gpt-5.4", slugs)                    # base kept

    def test_malformed_endpoint_skipped_not_fatal(self):
        # `add` rejects a base_url-less endpoint up front, so hand-edit one into the
        # registry to prove the MERGE also skips it defensively (never corrupts).
        self.add("good", {"base_url": "http://x", "model_id": "m"})
        with open(self.cfg) as f:
            cfg = json.load(f)
        cfg["endpoints"]["bad"] = {"model_id": "no base_url"}
        with open(self.cfg, "w") as f:
            json.dump(cfg, f)
        self.assertEqual(self.merge(), 0)
        slugs = {m["slug"] for m in self.catalog()["models"]}
        self.assertIn("self/good", slugs)
        self.assertNotIn("self/bad", slugs)                # skipped, didn't corrupt


class TestSchemaSafety(Base):
    def test_slim_entry_changes_only_intended_fields_vs_real_catalog(self):
        # Merge against the REAL dist catalog (which the engine ACCEPTS). Any enum
        # field accidentally set to an invalid variant (the apply_patch_tool_type=
        # "function" boot-brick) shows up as a diff on a field we didn't mean to
        # touch. Fields the slim template intentionally overrides are exempt; every
        # other field MUST equal the base entry's (engine-accepted) value.
        real = os.path.normpath(os.path.join(HERE, "..", "dist", "models-catalog.json"))
        if not os.path.exists(real):
            self.skipTest("dist catalog not present")
        import shutil
        shutil.copy(real, self.cat)
        base = next(m for m in json.load(open(self.cat))["models"] if m["slug"] == "gpt-5.4")
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        self.assertEqual(self.merge(), 0)
        e = next(m for m in self.catalog()["models"] if m["slug"] == "self/dgx")
        INTENDED = {"slug", "display_name", "description", "base_instructions", "model_messages",
                    "context_window", "max_context_window", "supports_reasoning_summaries",
                    "support_verbosity", "prefer_websockets", "supports_search_tool",
                    "include_skills_usage_instructions", "supported_reasoning_levels",
                    "service_tiers", "additional_speed_tiers", "visibility", "priority",
                    # The clone's template is gpt-5.4, so without an explicit
                    # override a user's OWN server would be filed under "OpenAI"
                    # in the two-level /model picker.
                    "picker_group"}
        for k, v in base.items():
            if k in INTENDED:
                continue
            self.assertEqual(e.get(k), v, "slim entry changed unintended field %r (base=%r new=%r) "
                             "- risks an invalid enum variant that bricks engine startup" % (k, v, e.get(k)))
        # the enum field we deliberately leave alone must keep the engine-accepted value
        self.assertEqual(e["apply_patch_tool_type"], base["apply_patch_tool_type"])


class TestPresets(Base):
    """Provider presets."""

    def test_table_shape(self):
        self.assertGreaterEqual(len(M.PROVIDER_PRESETS), 12)
        for pid, row in M.PROVIDER_PRESETS.items():
            url, wire, needs_key, key_env, kind = row       # 5-tuple, unpack must not raise
            self.assertTrue(url.startswith("http"), pid)
            self.assertIn(wire, ("chat", "auto"), pid)
            self.assertIn(kind, ("cloud", "local"), pid)
            if kind == "cloud":
                # 2A: cloud wire is PINNED (no billed probe) and every cloud preset
                # names its conventional key env var.
                self.assertEqual(wire, "chat", pid)
                self.assertTrue(needs_key, pid)
                self.assertTrue(key_env.endswith("_API_KEY"), pid)
                self.assertTrue(url.startswith("https://"), pid)
            else:
                self.assertFalse(needs_key, pid)

    def test_preset_subcommand_known(self):
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "preset", "openrouter"], capture_output=True)
        self.assertEqual(p.returncode, 0)
        pid, url, wire, needs_key, key_env, kind = p.stdout.decode().strip().split("|")
        self.assertEqual((pid, url, wire, needs_key, key_env, kind),
                         ("openrouter", "https://openrouter.ai/api/v1", "chat", "1",
                          "OPENROUTER_API_KEY", "cloud"))

    def test_preset_subcommand_unknown_errors(self):
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "preset", "nope"], capture_output=True)
        self.assertEqual(p.returncode, 1)
        self.assertIn(b"unknown provider", p.stderr)

    def test_presets_lists_all_sorted(self):
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "presets"], capture_output=True)
        self.assertEqual(p.returncode, 0)
        ids = [ln.split("|")[0] for ln in p.stdout.decode().strip().splitlines()]
        self.assertEqual(ids, sorted(M.PROVIDER_PRESETS))

    def test_version_subcommand(self):
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "version"], capture_output=True)
        self.assertEqual(p.returncode, 0)
        self.assertGreaterEqual(int(p.stdout.decode().strip()), 2)
        self.assertEqual(int(p.stdout.decode().strip()), M.MODELS_HELPER_VERSION)


class TestKeyEnv(Base):
    """3A: discover/probe-wire take the key via BK_KEY env (argv world-readable)."""

    def _serve(self, seen):
        import http.server
        import threading

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append(self.headers.get("Authorization") or "")
                body = json.dumps({"data": [{"id": "m1"}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass
        srv = http.server.HTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.shutdown)
        return "http://127.0.0.1:%d" % srv.server_address[1]

    def test_env_key_used_when_argv_empty(self):
        seen = []
        url = self._serve(seen)
        env = dict(os.environ, BK_KEY="sk-from-env")
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "discover", url], capture_output=True, env=env)
        self.assertEqual(p.returncode, 0, p.stderr.decode())
        self.assertEqual(p.stdout.decode().strip(), "m1")
        self.assertEqual(seen, ["Bearer sk-from-env"])

    def test_argv_key_still_works_and_wins(self):
        # REGRESSION: the old argv contract keeps working (old wrapper + new helper),
        # and an explicit argv key beats the env.
        seen = []
        url = self._serve(seen)
        env = dict(os.environ, BK_KEY="sk-from-env")
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "discover", url, "sk-from-argv"], capture_output=True, env=env)
        self.assertEqual(p.returncode, 0, p.stderr.decode())
        self.assertEqual(seen, ["Bearer sk-from-argv"])

    def test_discover_surfaces_http_status(self):
        # 4A: the failure class reaches stderr so the wrapper can map it to a hint.
        import http.server
        import threading

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(401)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *a):
                pass
        srv = http.server.HTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.shutdown)
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "discover", "http://127.0.0.1:%d" % srv.server_address[1]],
                           capture_output=True)
        self.assertEqual(p.returncode, 1)
        self.assertIn(b"HTTP 401", p.stderr)


class TestAutoHeal(Base):
    """Launch-time merge heals a preset endpoint whose url the table superseded."""

    def setUp(self):
        super().setUp()
        self._saved = M._SUPERSEDED_URLS
        M._SUPERSEDED_URLS = {"openrouter": ["https://old.openrouter.example/v1"]}

    def tearDown(self):
        M._SUPERSEDED_URLS = self._saved
        super().tearDown()

    def test_superseded_url_rewritten(self):
        self.add("or", {"base_url": "https://old.openrouter.example/v1", "model_id": "m",
                        "provider_id": "openrouter"})
        self.assertEqual(self.merge(), 0)
        cfg = json.load(open(self.cfg))
        self.assertEqual(cfg["endpoints"]["or"]["base_url"],
                         M.PROVIDER_PRESETS["openrouter"][0])

    def test_customized_url_never_touched(self):
        self.add("or", {"base_url": "https://my-proxy.corp/openrouter/v1", "model_id": "m",
                        "provider_id": "openrouter"})
        self.assertEqual(self.merge(), 0)
        cfg = json.load(open(self.cfg))
        self.assertEqual(cfg["endpoints"]["or"]["base_url"], "https://my-proxy.corp/openrouter/v1")

    def test_no_provider_id_never_touched(self):
        self.add("plain", {"base_url": "https://old.openrouter.example/v1", "model_id": "m"})
        self.assertEqual(self.merge(), 0)
        cfg = json.load(open(self.cfg))
        self.assertEqual(cfg["endpoints"]["plain"]["base_url"],
                         "https://old.openrouter.example/v1")

    def test_provider_id_survives_merge_and_slug_ok(self):
        self.add("or", {"base_url": "https://openrouter.ai/api/v1", "model_id": "openai/gpt-4o",
                        "provider_id": "openrouter"})
        self.assertEqual(self.merge(), 0)
        # slash in model_id must not leak into the slug (slug comes from the alias)
        slugs = {m["slug"] for m in self.catalog()["models"]}
        self.assertIn("self/or", slugs)
        cfg = json.load(open(self.cfg))
        self.assertEqual(cfg["endpoints"]["or"]["provider_id"], "openrouter")
        self.assertEqual(cfg["endpoints"]["or"]["model_id"], "openai/gpt-4o")


if __name__ == "__main__":
    unittest.main()


class TestToolsVerdict(Base):
    """`models test` records whether a self-hosted model can actually drive the
    agent loop, so the wrapper can warn at launch without re-running a billed turn.
    Live 2026-07-24: qwen2.5-coder:3b passed the explicit-call check 5/6 but emitted
    a call-shaped JSON blob for a plain "hi" 5/5, so the greeting check is the real
    discriminator (llama3.1:8b on the same host: 0/5)."""

    def test_call_blob_detected(self):
        for s in ('{"name": "get_time", "arguments": {}}',
                  '```json\n{"name": "", "arguments": {}}\n```',
                  '{}',
                  '<tool_call>{"name":"x"}</tool_call>'):
            self.assertTrue(M._looks_like_call_blob(s), s)

    def test_prose_is_not_a_call_blob(self):
        for s in ("Hi! How can I assist you today?", "", "   ",
                  "Sure - here is a JSON example: {\"a\": 1}"):
            self.assertFalse(M._looks_like_call_blob(s), repr(s))

    def test_verdict_persisted_on_endpoint(self):
        self.add("m", {"base_url": "http://x", "model_id": "m"})
        M._record_tools_ok(self.cfg, "m", False)
        self.assertIs(json.load(open(self.cfg))["endpoints"]["m"]["tools_ok"], False)
        M._record_tools_ok(self.cfg, "m", True)
        self.assertIs(json.load(open(self.cfg))["endpoints"]["m"]["tools_ok"], True)

    def test_verdict_keeps_other_endpoint_fields(self):
        self.add("m", {"base_url": "http://x", "model_id": "m", "wire": "chat",
                       "api_key": "k"})
        M._record_tools_ok(self.cfg, "m", False)
        ep = json.load(open(self.cfg))["endpoints"]["m"]
        self.assertEqual(ep["api_key"], "k")
        self.assertEqual(ep["wire"], "chat")

    def test_verdict_for_unknown_alias_is_a_noop(self):
        self.add("m", {"base_url": "http://x", "model_id": "m"})
        M._record_tools_ok(self.cfg, "ghost", False)
        self.assertNotIn("ghost", json.load(open(self.cfg))["endpoints"])

    def test_get_exposes_verdict_to_the_wrapper(self):
        # The sh wrapper reads this exact shape: a bare `false` on stdout.
        self.add("m", {"base_url": "http://x", "model_id": "m"})
        M._record_tools_ok(self.cfg, "m", False)
        p = subprocess.run([sys.executable, os.path.join(HERE, "byteask_models.py"),
                            "get", self.cfg, "m", "tools_ok"], capture_output=True)
        self.assertEqual(p.stdout.decode().strip(), "false")


class TestSelfHostedNoEffortSubmenu(Base):
    """A self/<alias> entry clones gpt-5.4, which DOES carry reasoning levels.
    Inheriting them would put an effort submenu on the user's own server, where
    the knob does nothing -- the same lying-control problem as inheriting the
    'OpenAI' picker group. Both must be explicitly stripped."""

    def test_slim_entry_strips_reasoning_levels_and_gate(self):
        real = os.path.normpath(os.path.join(HERE, "..", "dist", "models-catalog.json"))
        if not os.path.exists(real):
            self.skipTest("dist catalog not present")
        import shutil
        shutil.copy(real, self.cat)
        base = next(m for m in json.load(open(self.cat))["models"] if m["slug"] == "gpt-5.4")
        # guard the premise: the template really does carry levels
        self.assertTrue(base["supported_reasoning_levels"])
        self.assertTrue(base["supports_reasoning_summaries"])
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        self.assertEqual(self.merge(), 0)
        e = next(m for m in self.catalog()["models"] if m["slug"] == "self/dgx")
        self.assertEqual(e["supported_reasoning_levels"], [])
        self.assertIs(e["supports_reasoning_summaries"], False)

    def test_slim_entry_files_under_self_hosted_not_openai(self):
        real = os.path.normpath(os.path.join(HERE, "..", "dist", "models-catalog.json"))
        if not os.path.exists(real):
            self.skipTest("dist catalog not present")
        import shutil
        shutil.copy(real, self.cat)
        self.add("dgx", {"base_url": "http://x", "model_id": "m"})
        self.assertEqual(self.merge(), 0)
        e = next(m for m in self.catalog()["models"] if m["slug"] == "self/dgx")
        self.assertEqual(e["picker_group"], "Self-hosted")
