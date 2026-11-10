#!/usr/bin/env python3
"""Client-side helper for self-hosted / custom models (`byteask models ...`).

Shipped to ~/.byteask beside byok_sidecar.py and driven by BOTH the sh and ps1
wrappers, so the heavy logic lives in ONE place (pure stdlib):

  * the endpoint REGISTRY in byok-config.json  — atomic (temp+fsync+replace) so a
    per-request sidecar read never sees a torn file; a POSIX flock guards two
    concurrent writers.
  * the CATALOG MERGE — inject one `self/<alias>` picker row per endpoint using a
    SLIM template (no Codex/GPT identity, function-type apply_patch, GPT-only knobs
    off), atomically, NEVER dropping a base cloud model (a bad catalog bricks the
    engine, so we validate-before-replace and keep the last-good file).
  * DISCOVERY / wire PROBE / capability TEST against the user's endpoint.

Reuses byok_sidecar's endpoint helpers (normalize/ssl/headers/conn) — one codebase.

Subcommands (argv[1]):
  merge-catalog <config> <catalog>
  merge-access <access.json> <catalog> [byok-config.json]
  add <config> <alias>            (endpoint JSON object on stdin)
  remove <config> <alias>
  get <config> <alias> <field>
  list <config> [--json]
  discover <base_url> [api_key] [ca_bundle] [insecure]
  probe-wire <base_url> <model_id> [api_key] [ca_bundle] [insecure]
  test <config> <alias>
  version                          (prints MODELS_HELPER_VERSION)
  presets                          (one preset per line: id|url|wire|needs_key|key_env|kind)
  preset <id>                      (that one line, or exit 1 on unknown id)
Prints results to stdout; errors to stderr with a non-zero exit.
For discover/probe-wire the API key may also come via the BK_KEY env var (preferred
over argv — /proc/*/cmdline is world-readable); an argv key still wins if given.
"""
import copy
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import byok_sidecar as _sc          # reuse _normalize_base/_ssl_ctx_for/_endpoint_headers/_conn_for/_self_path
import openai_compat_translate as _t

try:
    import fcntl                     # POSIX only; absent on Windows (best-effort lock)
except Exception:
    fcntl = None

# Bumped whenever a wrapper-visible subcommand is added/changed, so a NEW wrapper can
# detect an OLD helper (installers fetch helper files fail-soft) and say "run
# byteask --update" instead of crashing on a missing subcommand. Mirrors the
# sidecar's PROTOCOL_VERSION pattern. 2 = presets/preset/version + BK_KEY env.
# 3 = merge-access (per-account model availability stamped onto the catalog).
MODELS_HELPER_VERSION = 3

# Vendored provider presets. Cloud
# entries are BYOK-direct: the user's key, the provider bills them, traffic never
# touches the ByteAsk gateway. wire is PINNED for cloud (all Chat-Completions
# today) so `models add --provider X` never fires a billed wire probe; local
# entries keep "auto" (a local server costs nothing to probe).
# id: (base_url, wire, needs_key, key_env, kind)
PROVIDER_PRESETS = {
    "openrouter": ("https://openrouter.ai/api/v1",          "chat", True,  "OPENROUTER_API_KEY", "cloud"),
    "groq":       ("https://api.groq.com/openai/v1",        "chat", True,  "GROQ_API_KEY",       "cloud"),
    "together":   ("https://api.together.xyz/v1",           "chat", True,  "TOGETHER_API_KEY",   "cloud"),
    "deepseek":   ("https://api.deepseek.com/v1",           "chat", True,  "DEEPSEEK_API_KEY",   "cloud"),
    "mistral":    ("https://api.mistral.ai/v1",             "chat", True,  "MISTRAL_API_KEY",    "cloud"),
    "xai":        ("https://api.x.ai/v1",                   "chat", True,  "XAI_API_KEY",        "cloud"),
    "cerebras":   ("https://api.cerebras.ai/v1",            "chat", True,  "CEREBRAS_API_KEY",   "cloud"),
    "fireworks":  ("https://api.fireworks.ai/inference/v1", "chat", True,  "FIREWORKS_API_KEY",  "cloud"),
    "ollama":     ("http://localhost:11434",                "auto", False, "",                   "local"),
    "lmstudio":   ("http://localhost:1234",                 "auto", False, "",                   "local"),
    "llamacpp":   ("http://localhost:8080",                 "auto", False, "",                   "local"),
    "vllm":       ("http://localhost:8000",                 "auto", False, "",                   "local"),
}

# URL-move policy: when a preset's base_url changes, move the OLD url here so
# the launch-time merge can auto-heal endpoints that still point at it. Only an
# EXACT match on a superseded url is rewritten — a user-customized url never
# matches, so it is never touched.
_SUPERSEDED_URLS = {
    # "example-provider": ["https://old.example.com/v1"],
}

# Slim system prompt: keeps the tool + apply_patch CONTRACT, drops all
# Codex/GPT identity and the 12KB frontier prose. This is what a self/* model
# actually receives (it goes DIRECT to the user's box, bypassing the gateway scrub).
SELF_BASE_INSTRUCTIONS = (
    "You are ByteAsk, a coding assistant running in the ByteAsk CLI. You help with "
    "software engineering: reading, writing and editing code, running commands, and "
    "debugging.\n\n"
    "Use tools by emitting a tool call with valid JSON arguments — never describe the "
    "call in prose. Core tools:\n"
    "- apply_patch: create or edit files. Put the full patch in the `input` argument:\n"
    "    *** Begin Patch\n"
    "    *** Add File: path/to/file.py\n"
    "    +<new line>\n"
    "    *** End Patch\n"
    "  To edit an existing file use `*** Update File: <path>` with @@ context and +/- lines.\n"
    "- shell / exec: run a shell command, then read its output before continuing.\n\n"
    "Be concise and precise. Make minimal, correct changes and verify them when you can."
)


# --------------------------------------------------------------------------- #
# registry (atomic)                                                           #
# --------------------------------------------------------------------------- #
def _read(config):
    try:
        with open(config, "rb") as f:
            cfg = json.loads(f.read() or b"{}")
        return cfg if isinstance(cfg, dict) else {}
    except FileNotFoundError:
        return {}
    except Exception:
        return {}


def _atomic_write(path, cfg, mode=0o600):
    d = os.path.dirname(path) or "."
    os.makedirs(d, exist_ok=True)
    tmp = path + ".tmp.%d" % os.getpid()
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
    try:
        os.write(fd, json.dumps(cfg, indent=1).encode())
        os.fsync(fd)
    finally:
        os.close(fd)
    try:
        os.chmod(tmp, mode)
    except Exception:
        pass
    os.replace(tmp, path)            # atomic: a concurrent reader sees old OR new, never torn


def _with_lock(config, fn):
    """Run fn() holding a cross-process lock on <config>.lock (POSIX). Windows has
    no fcntl → run unlocked (the atomic replace still prevents torn reads)."""
    if fcntl is None:
        return fn()
    lockp = config + ".lock"
    os.makedirs(os.path.dirname(config) or ".", exist_ok=True)
    lf = open(lockp, "w")
    try:
        fcntl.flock(lf, fcntl.LOCK_EX)
        return fn()
    finally:
        try:
            fcntl.flock(lf, fcntl.LOCK_UN)
        finally:
            lf.close()


def cmd_add(config, alias):
    ep = json.loads(sys.stdin.read() or "{}")
    if not isinstance(ep, dict) or not str(ep.get("base_url") or "").strip():
        sys.stderr.write("add: endpoint needs a base_url\n")
        return 2

    def _do():
        cfg = _read(config)
        cfg.setdefault("endpoints", {})
        cfg["endpoints"][alias] = ep
        _atomic_write(config, cfg)
    _with_lock(config, _do)
    print("ok")
    return 0


def cmd_remove(config, alias):
    removed = [False]

    def _do():
        cfg = _read(config)
        eps = cfg.get("endpoints") or {}
        if alias in eps:
            del eps[alias]
            cfg["endpoints"] = eps
            _atomic_write(config, cfg)
            removed[0] = True
    _with_lock(config, _do)
    print("removed" if removed[0] else "notfound")
    return 0


def cmd_get(config, alias, field):
    ep = (_read(config).get("endpoints") or {}).get(alias) or {}
    v = ep.get(field, "")
    print(v if isinstance(v, str) else json.dumps(v))
    return 0


def cmd_list(config, as_json=False):
    eps = _read(config).get("endpoints") or {}
    if as_json:
        print(json.dumps(eps))
        return 0
    if not eps:
        print("(no self-hosted models registered — add one with: byteask models add)")
        return 0
    for alias in sorted(eps):
        ep = eps[alias]
        host = _host(ep.get("base_url"))
        print("  self/%-20s %-8s tools=%-6s %s" % (
            alias, ep.get("wire") or "auto", ep.get("tools") or "auto", host))
    return 0


def cmd_version():
    print(MODELS_HELPER_VERSION)
    return 0


def _preset_line(pid):
    url, wire, needs_key, key_env, kind = PROVIDER_PRESETS[pid]
    return "%s|%s|%s|%s|%s|%s" % (pid, url, wire, "1" if needs_key else "0", key_env, kind)


def cmd_presets():
    for pid in sorted(PROVIDER_PRESETS):
        print(_preset_line(pid))
    return 0


def cmd_preset(pid):
    pid = (pid or "").strip().lower()
    if pid not in PROVIDER_PRESETS:
        sys.stderr.write("preset: unknown provider %r — see: byteask models providers\n" % pid)
        return 1
    print(_preset_line(pid))
    return 0


def _host(url):
    try:
        import urllib.parse
        u = urllib.parse.urlparse(_sc._normalize_base(url or ""))
        return u.netloc or url or ""
    except Exception:
        return url or ""


# --------------------------------------------------------------------------- #
# catalog merge (boot-critical: validate before replacing)                    #
# --------------------------------------------------------------------------- #
def _slim_entry(base_entry, ep, alias, priority):
    """Type-preserving overrides on a cloned base cloud entry (same invariant as
    the catalog generator: every override matches the base value's JSON type, so the
    result always parses as a ModelsResponse and can't brick engine startup)."""
    e = copy.deepcopy(base_entry)
    e["slug"] = "self/" + alias
    e["display_name"] = str(ep.get("display_name") or alias)
    e["description"] = str(ep.get("display_name") or ("Self-hosted model: " + alias))
    e["base_instructions"] = SELF_BASE_INSTRUCTIONS
    if isinstance(e.get("model_messages"), dict):
        e["model_messages"] = dict(e["model_messages"], instructions_template=SELF_BASE_INSTRUCTIONS)
    # NB: `apply_patch_tool_type` is a single-variant engine enum (`freeform` ONLY —
    # protocol/src/openai_models.rs). Any other value ("function") makes the catalog
    # fail to parse and BRICKS engine startup. So we KEEP the cloned base value
    # (freeform) and never override it. (A "function form" isn't a thing the
    # engine's catalog exposes; open-model apply_patch quality is a documented caveat.)
    ctx = int(ep.get("context_window") or 32768)
    if "context_window" in e:
        e["context_window"] = ctx
    if "max_context_window" in e:
        e["max_context_window"] = ctx
    for k in ("supports_reasoning_summaries", "support_verbosity", "prefer_websockets",
              "supports_search_tool", "include_skills_usage_instructions"):
        if k in e:
            e[k] = False
    for k in ("supported_reasoning_levels", "service_tiers", "additional_speed_tiers"):
        if k in e:
            e[k] = []
    if "visibility" in e:
        e["visibility"] = "list"
    # The clone inherits the template's `/model` menu group (gpt-5.4 -> "OpenAI"), which
    # would file the user's OWN server under OpenAI. These are always self-hosted.
    e["picker_group"] = "Self-hosted"
    e["priority"] = int(priority)
    return e


def _heal_preset_urls(config):
    """Auto-heal: an endpoint added from a preset stores provider_id; if a later
    helper update moved that preset's base_url (old url recorded in
    _SUPERSEDED_URLS), rewrite endpoints still pointing at the EXACT old url. A
    user-customized url never matches, so it is never touched. Best-effort: any
    error leaves the registry as-is."""
    try:
        healed = []

        def _do():
            cfg = _read(config)
            eps = cfg.get("endpoints") or {}
            changed = False
            for alias, ep in eps.items():
                if not isinstance(ep, dict):
                    continue
                pid = ep.get("provider_id")
                if pid not in PROVIDER_PRESETS:
                    continue
                old_urls = _SUPERSEDED_URLS.get(pid) or []
                if ep.get("base_url") in old_urls:
                    new_url = PROVIDER_PRESETS[pid][0]
                    healed.append((alias, ep.get("base_url"), new_url))
                    ep["base_url"] = new_url
                    changed = True
            if changed:
                _atomic_write(config, cfg)
        _with_lock(config, _do)
        for alias, old, new in healed:
            sys.stderr.write("models: healed self/%s url %s -> %s (provider moved)\n"
                             % (alias, old, new))
    except Exception:
        pass


def cmd_merge_catalog(config, catalog):
    """Idempotent: strip any prior self/ rows, re-add from the registry. Preserve
    EVERY base cloud model. Validate the result, then atomically replace. On any
    failure, leave the existing catalog untouched (keep-last-good)."""
    _heal_preset_urls(config)
    eps = _read(config).get("endpoints") or {}
    try:
        with open(catalog, "rb") as f:
            cat = json.loads(f.read() or b"{}")
    except Exception as e:
        sys.stderr.write("merge-catalog: base catalog unreadable (%s); left unchanged\n" % e)
        return 1
    models = cat.get("models") if isinstance(cat, dict) else None
    if not isinstance(models, list) or not models:
        sys.stderr.write("merge-catalog: base catalog has no models; left unchanged\n")
        return 1

    base_models = [m for m in models if isinstance(m, dict)
                   and not str(m.get("slug") or "").startswith("self/")]
    base_slugs = {m.get("slug") for m in base_models}
    if not eps:
        # No endpoints: ensure the catalog carries no stale self/ rows, else leave as-is.
        if len(base_models) == len(models):
            return 0
        out = dict(cat, models=base_models)
    else:
        template = next((m for m in base_models if m.get("slug") == "gpt-5.4"),
                        base_models[0] if base_models else None)
        if template is None:
            sys.stderr.write("merge-catalog: no base model to clone; left unchanged\n")
            return 1
        extra = []
        for i, alias in enumerate(sorted(eps)):
            ep = eps[alias]
            if not isinstance(ep, dict) or not str(ep.get("base_url") or "").strip():
                continue
            extra.append(_slim_entry(template, ep, alias, 90 + i))
        out = dict(cat, models=base_models + extra)

    # Validate before replacing: must round-trip as JSON AND keep every base slug.
    try:
        json.loads(json.dumps(out))
    except Exception as e:
        sys.stderr.write("merge-catalog: produced invalid JSON (%s); left unchanged\n" % e)
        return 1
    out_slugs = {m.get("slug") for m in out["models"]}
    if not base_slugs.issubset(out_slugs):
        sys.stderr.write("merge-catalog: refused (would drop a base model); left unchanged\n")
        return 1
    try:
        _atomic_write(catalog, out, mode=0o644)
    except Exception as e:
        sys.stderr.write("merge-catalog: write failed (%s); left unchanged\n" % e)
        return 1
    return 0


def _keyed_providers(cfg):
    """Providers the user has saved their OWN key for (byok-config.json `keys`).
    Any failure -> empty set, i.e. the existing locks apply unchanged."""
    if not cfg:
        return set()
    try:
        with open(cfg, "rb") as f:
            keys = (json.loads(f.read() or b"{}") or {}).get("keys") or {}
        return {p for p, v in keys.items() if isinstance(v, str) and v.strip()}
    except Exception:
        return set()


def cmd_merge_access(access, catalog, cfg=None):
    """Stamp this account's availability overlay onto the local catalog.

    `access` is the gateway's ~108-byte answer:
        {"locked": ["claude-opus-5", ...], "inactive": ["gemini-3.6-flash", ...]}

    Two different states, deliberately rendered differently:
      * locked   -> `locked_reason` on the row. A paywall: the row stays visible so the
        user can see what upgrading buys, and the engine draws it unselectable.
      * inactive -> `visibility: "hide"`. An operator supply decision that applies to
        paying users too, so it is removed rather than advertised — offering to sell an
        upgrade for a model we switched off would tell a subscriber to buy what they
        already own.

    IDEMPOTENT. Every run first clears the marks it previously set, so an upgrade (or a
    model coming back) unlocks rows instead of leaving them stuck. That is why the clear
    is unconditional and not gated on the overlay being non-empty.

    Runs on the launch path, so it inherits merge-catalog's discipline exactly: validate,
    refuse to drop a base model, atomic replace, and on ANY failure leave the existing
    catalog untouched. A catalog this corrupts is a client that cannot pick a model.

    ⚠️ `self/*` rows are never touched. A user's own endpoint is their business; we do
    not pay for it and must not gate it.

    ⚠️ Same reasoning for a provider the user saved their OWN key for (optional `cfg` =
    byok-config.json): those calls go direct to the provider and cost us nothing, so a
    paywall on them only blocks the user. `locked` rows of a keyed provider are left
    unlocked. `inactive` (a supply decision) still hides. A missing/unreadable `cfg`
    means no key, i.e. today's behaviour."""
    try:
        with open(access, "rb") as f:
            acc = json.loads(f.read() or b"{}")
    except Exception as e:
        sys.stderr.write("merge-access: overlay unreadable (%s); left unchanged\n" % e)
        return 1
    if not isinstance(acc, dict):
        sys.stderr.write("merge-access: overlay is not an object; left unchanged\n")
        return 1
    locked = {s for s in (acc.get("locked") or []) if isinstance(s, str)}
    inactive = {s for s in (acc.get("inactive") or []) if isinstance(s, str)}

    try:
        with open(catalog, "rb") as f:
            cat = json.loads(f.read() or b"{}")
    except Exception as e:
        sys.stderr.write("merge-access: base catalog unreadable (%s); left unchanged\n" % e)
        return 1
    models = cat.get("models") if isinstance(cat, dict) else None
    if not isinstance(models, list) or not models:
        sys.stderr.write("merge-access: base catalog has no models; left unchanged\n")
        return 1

    keyed = _keyed_providers(cfg)
    base_slugs = {m.get("slug") for m in models if isinstance(m, dict)}
    out_models = []
    for m in models:
        if not isinstance(m, dict):
            out_models.append(m)
            continue
        slug = str(m.get("slug") or "")
        if slug.startswith("self/"):
            out_models.append(m)
            continue
        row = dict(m)
        # Clear our own prior marks before re-applying — this is what makes an upgrade
        # take effect rather than leaving every row locked forever.
        row.pop("locked_reason", None)
        if row.get("visibility") == "hide" and row.get("byteask_hidden_by_access"):
            row.pop("visibility", None)
            row.pop("byteask_hidden_by_access", None)
        if slug in inactive:
            row["visibility"] = "hide"
            row["byteask_hidden_by_access"] = True
        elif slug in locked and _sc._provider_of(slug) not in keyed:
            row["locked_reason"] = "Available on ByteAsk Pro"
        out_models.append(row)

    out = dict(cat, models=out_models)
    try:
        json.loads(json.dumps(out))
    except Exception as e:
        sys.stderr.write("merge-access: produced invalid JSON (%s); left unchanged\n" % e)
        return 1
    if not base_slugs.issubset({m.get("slug") for m in out["models"] if isinstance(m, dict)}):
        sys.stderr.write("merge-access: refused (would drop a model); left unchanged\n")
        return 1
    try:
        _atomic_write(catalog, out, mode=0o644)
    except Exception as e:
        sys.stderr.write("merge-access: write failed (%s); left unchanged\n" % e)
        return 1
    return 0


# --------------------------------------------------------------------------- #
# discovery / probe / test against the user's endpoint                        #
# --------------------------------------------------------------------------- #
def _ep_from_args(base_url, api_key="", ca_bundle="", insecure=""):
    # 3A: prefer the BK_KEY env var over an argv key (argv is world-readable via
    # /proc/*/cmdline while the process runs); argv stays as a fallback so an old
    # wrapper calling the new helper keeps working for one release.
    api_key = api_key or os.environ.get("BK_KEY", "")
    ep = {"base_url": base_url}
    if api_key:
        ep["api_key"] = api_key
    if ca_bundle:
        ep["ca_bundle"] = ca_bundle
    if insecure and insecure not in ("0", "false", "False", ""):
        ep["insecure"] = True
    return ep


def _request(base, path, method="GET", body=None, ep=None, stream=False, timeout=15):
    base = _sc._normalize_base(base)
    ctx = _sc._ssl_ctx_for(base, ep or {})
    conn, _ = _sc._conn_for(base, ctx)
    conn.timeout = timeout
    headers = _sc._endpoint_headers(ep or {}, stream=stream)
    conn.request(method, _sc._self_path(base, path), body=body, headers=headers)
    return conn, conn.getresponse()


def cmd_discover(base_url, api_key="", ca_bundle="", insecure=""):
    ep = _ep_from_args(base_url, api_key, ca_bundle, insecure)
    try:
        conn, resp = _request(base_url, "models", "GET", None, ep, timeout=10)
        data = resp.read()
        conn.close()
    except Exception as e:
        sys.stderr.write("discover: %s\n" % e)
        return 1
    if resp.status != 200:
        sys.stderr.write("discover: HTTP %d from /v1/models\n" % resp.status)
        return 1
    try:
        ids = [m.get("id") for m in (json.loads(data).get("data") or []) if m.get("id")]
    except Exception:
        ids = []
    for i in ids:
        print(i)
    return 0


def cmd_probe_wire(base_url, model_id, api_key="", ca_bundle="", insecure=""):
    """Print 'responses' if the server answers /v1/responses, else 'chat'."""
    ep = _ep_from_args(base_url, api_key, ca_bundle, insecure)
    body = json.dumps({"model": model_id, "stream": True, "max_output_tokens": 1,
                       "input": [{"type": "message", "role": "user",
                                  "content": [{"type": "input_text", "text": "hi"}]}]}).encode()
    try:
        conn, resp = _request(base_url, "responses", "POST", body, ep, stream=True, timeout=12)
        ok = resp.status == 200
        conn.close()
    except Exception:
        ok = False
    print("responses" if ok else "chat")
    return 0


def cmd_test(config, alias):
    """Plain-English capability check (non-mutating): reachable? model valid?
    streaming? does a tool call round-trip? Mirrors the spike's matrix."""
    ep = (_read(config).get("endpoints") or {}).get(alias)
    if not ep:
        print("unknown self-hosted model '%s' — add it first." % alias)
        return 1
    base = ep.get("base_url")
    model_id = ep.get("model_id") or alias
    wire = (ep.get("wire") or "auto").lower()
    rows = []

    # 1) reachable + model list
    try:
        conn, resp = _request(base, "models", "GET", None, ep, timeout=8)
        ids = [m.get("id") for m in (json.loads(resp.read()).get("data") or [])]
        conn.close()
        rows.append(("reachable", resp.status == 200, "%d model(s)" % len(ids)))
        rows.append(("model id valid", model_id in ids if ids else True,
                     model_id if (not ids or model_id in ids) else "%s not in %s" % (model_id, ids)))
    except Exception as e:
        rows.append(("reachable", False, str(e)[:60]))
        _print_matrix(alias, rows)
        return 1

    # 2) a text turn + 3) a non-mutating tool round-trip, through the translator
    tool = {"type": "function", "name": "get_time", "description": "Return the current time.",
            "parameters": {"type": "object", "properties": {}, "required": []}}
    req = {"instructions": "Use the get_time tool to answer.", "tools": [tool],
           "input": [{"type": "message", "role": "user",
                      "content": [{"type": "input_text", "text": "What time is it? Call get_time."}]}]}
    text, calls, ok, err = _run_turn(base, model_id, req, ep, wire)
    rows.append(("streaming", ok, err or (text[:40] or "(tool call)")))
    rows.append(("tool round-trip", bool(calls),
                 (calls[0] if calls else "model replied text, not a structured tool call")))

    # 4) the harder half of tool use: STAYING QUIET. A model that emits a call-shaped
    # blob when no tool is needed dumps raw JSON on screen for an ordinary greeting.
    # This discriminates far better than (3) alone: measured on one Ollama host,
    # qwen2.5-coder:3b passed (3) 5/6 but failed this 5/5, while llama3.1:8b passed both.
    greet = {"instructions": "You are ByteAsk, a coding agent.", "tools": [tool],
             "input": [{"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}]}]}
    gtext, gcalls, gok, gerr = _run_turn(base, model_id, greet, ep, wire)
    clean = gok and not gcalls and not _looks_like_call_blob(gtext)
    rows.append(("no false tool calls", clean,
                 "replies in prose" if clean else
                 ("called %s for a greeting" % gcalls[0] if gcalls
                  else "emitted raw JSON: %s" % (gtext.strip()[:48] or "(empty)"))))

    _record_tools_ok(config, alias, bool(calls) and clean)
    _print_matrix(alias, rows)
    if not calls or not clean:
        # Agent work is a tool loop, so a model that can't emit a structured call
        # is unusable for it however good its prose is. Say so plainly here, and
        # persist it so the wrapper can repeat the warning at launch.
        print("    note: %s is not reliable at tool calling, so file edits, shell" % model_id)
        print("          commands and code search may not work, and it can print raw JSON")
        print("          at you instead of answering. Models under ~7B are usually too")
        print("          small for the agent loop; a larger one is the fix.")
    return 0 if all(r[1] for r in rows[:3]) else 1


def _looks_like_call_blob(text):
    """True when a reply is a JSON/XML tool-call blob rather than prose. Used only
    to judge a greeting, where any such blob is a defect however well-formed."""
    s = _t._strip_fence(text or "")
    return bool(s) and (s[0] in "{[" or s.lower().startswith("<tool"))


def _record_tools_ok(config, alias, ok):
    """Persist the tool round-trip verdict on the endpoint so the wrapper can warn
    at launch without re-running a billed turn. Best-effort: never fails a test."""
    def _do():
        cfg = _read(config)
        eps = cfg.get("endpoints") or {}
        if alias in eps and isinstance(eps[alias], dict):
            eps[alias]["tools_ok"] = bool(ok)
            cfg["endpoints"] = eps
            _atomic_write(config, cfg)
    try:
        _with_lock(config, _do)
    except Exception:
        pass


def _run_turn(base, model_id, req, ep, wire):
    """One turn against the endpoint via the real translator. Returns
    (text, [tool names], ok, error)."""
    use_chat = wire != "responses"
    try:
        if use_chat:
            chat_body, name_map = _t.translate_request(dict(req, model=model_id), model_id)
            conn, resp = _request(base, "chat/completions", "POST",
                                  json.dumps(chat_body).encode(), ep, stream=True, timeout=60)
        else:
            conn, resp = _request(base, "responses", "POST",
                                  json.dumps(dict(req, model=model_id, stream=True)).encode(),
                                  ep, stream=True, timeout=60)
            name_map = {}
        if resp.status != 200:
            body = resp.read()[:200].decode("utf-8", "replace")
            conn.close()
            return "", [], False, "HTTP %d: %s" % (resp.status, body)
        text, calls = "", []
        if use_chat:
            td = _t.Transducer(
                model_id, name_map,
                known_tools={t["function"]["name"] for t in chat_body.get("tools") or []},
                custom_tools=_t.custom_tool_names(req.get("tools")))
            frames = []
            for ev in _t.iter_sse(iter(lambda: resp.read(2048), b"")):
                frames += td.feed(ev)
            frames += td.close()
            for f in frames:
                for ln in f.split("\n"):
                    if ln.startswith("data: "):
                        d = json.loads(ln[6:])
                        if d.get("type") == "response.output_text.delta":
                            text += d.get("delta", "")
                        elif (d.get("type") == "response.output_item.done"
                              and (d.get("item") or {}).get("type")
                              in ("function_call", "custom_tool_call")):
                            calls.append(d["item"]["name"])
        else:
            raw = resp.read().decode("utf-8", "replace")
            text = "(responses passthrough ok)" if "response.completed" in raw else ""
            if "function_call" in raw:
                calls.append("(tool call)")
        conn.close()
        return text, calls, True, ""
    except Exception as e:
        return "", [], False, str(e)[:80]


def _print_matrix(alias, rows):
    print("  self/%s:" % alias)
    for name, ok, detail in rows:
        print("    [%s] %-16s %s" % ("PASS" if ok else "FAIL", name, detail))


# --------------------------------------------------------------------------- #
def main(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: byteask_models.py <subcommand> ...\n")
        return 2
    cmd, rest = argv[1], argv[2:]
    try:
        if cmd == "merge-catalog":
            return cmd_merge_catalog(rest[0], rest[1])
        if cmd == "merge-access":
            return cmd_merge_access(rest[0], rest[1], rest[2] if len(rest) > 2 else None)
        if cmd == "add":
            return cmd_add(rest[0], rest[1])
        if cmd == "remove":
            return cmd_remove(rest[0], rest[1])
        if cmd == "get":
            return cmd_get(rest[0], rest[1], rest[2])
        if cmd == "list":
            return cmd_list(rest[0], "--json" in rest[1:])
        if cmd == "discover":
            return cmd_discover(*rest[:4])
        if cmd == "probe-wire":
            return cmd_probe_wire(*rest[:5])
        if cmd == "test":
            return cmd_test(rest[0], rest[1])
        if cmd == "version":
            return cmd_version()
        if cmd == "presets":
            return cmd_presets()
        if cmd == "preset":
            return cmd_preset(rest[0])
    except IndexError:
        sys.stderr.write("byteask_models.py %s: missing argument\n" % cmd)
        return 2
    sys.stderr.write("byteask_models.py: unknown subcommand %r\n" % cmd)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
