#!/bin/sh
# byteask models — end-to-end CLI test (self-hosted / custom endpoints).
# Drives the real wrapper against a temp BYTEASK_HOME. Endpoint-less paths (add with an
# explicit --model, list, remove, catalog merge, config-mode flip, --update survival)
# always run; discovery / wire-probe / `models test` additionally run if a local
# Ollama (127.0.0.1:11434) is reachable.
set -eu
HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$HERE/.." && pwd)"
WRAPPER="$ROOT/byteask"
GW="$ROOT/helpers"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok  %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf 'FAIL  %s\n' "$1"; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
export BYTEASK_HOME="$TMP/home"; mkdir -p "$BYTEASK_HOME"
export BYTEASK_NO_UPDATE_CHECK=1
export BYOK_SKIP_SIDECAR=1                      # don't spawn a real sidecar in the CLI test
# Ship the helper + sidecar + translators into the test home (as install.sh would).
# Keep in sync with install.sh's fetch list. effort.py is a module-level dependency of
# the translators, so omitting it makes `import anthropic_translate` raise and every
# models command fail with "failed to save the endpoint" (all 7 checks below cascade
# off that).
for f in byok_sidecar.py anthropic_translate.py gemini_translate.py \
         openai_compat_translate.py byteask_models.py effort.py; do
  cp "$GW/$f" "$BYTEASK_HOME/$f"
done
# A signed-in managed config + a base catalog (so _byok_enter has a JWT + a merge base).
cp "$ROOT/dist/models-catalog.json" "$BYTEASK_HOME/models-catalog.json"
cat > "$BYTEASK_HOME/byok-config.json" <<EOF
{"keys":{},"jwt":"eyJ.header.sig","local_token":"tok-test","gateway":"https://code.byteask.ai"}
EOF
cat > "$BYTEASK_HOME/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
model_catalog_json = "$BYTEASK_HOME/models-catalog.json"
EOF

run() { BYTEASK_HOME="$BYTEASK_HOME" sh "$WRAPPER" "$@"; }

echo "== add with explicit model (no live endpoint needed) =="
run models add my-dgx --url http://dgx-01.lan:8000 --model Qwen2.5-Coder-32B --wire chat --ctx 32768 --name "Qwen DGX" >/dev/null 2>&1 \
  && ok "models add succeeds" || bad "models add"
python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" my-dgx model_id | grep -q Qwen2.5-Coder-32B \
  && ok "registry stored the model_id" || bad "registry model_id"
python3 -c "import json;c=json.load(open('$BYTEASK_HOME/models-catalog.json'));print('self/my-dgx' in [m['slug'] for m in c['models']])" | grep -q True \
  && ok "catalog gained self/my-dgx" || bad "catalog merge"
python3 -c "import json;c=json.load(open('$BYTEASK_HOME/models-catalog.json'));e=next(m for m in c['models'] if m['slug']=='self/my-dgx');import sys;sys.exit(0 if 'Codex' not in e.get('base_instructions','') and e.get('apply_patch_tool_type')=='freeform' else 1)" \
  && ok "slim template (no Codex; apply_patch_tool_type=freeform, engine-valid)" || bad "slim template"
grep -q '^model_provider = "byok-local"' "$BYTEASK_HOME/config.toml" \
  && ok "forced byok-local config mode" || bad "config mode not flipped"
python3 -c "import json;c=json.load(open('$BYTEASK_HOME/models-catalog.json'));import sys;sys.exit(0 if 'gpt-5.4' in [m['slug'] for m in c['models']] else 1)" \
  && ok "base gpt-5.4 preserved" || bad "base dropped"

echo "== list =="
run models list 2>/dev/null | grep -q "self/my-dgx" && ok "models list shows it" || bad "models list"

echo "== --update survival: overwrite catalog with the base, launch-merge restores self row =="
cp "$ROOT/dist/models-catalog.json" "$BYTEASK_HOME/models-catalog.json"     # simulate --update
python3 "$GW/byteask_models.py" merge-catalog "$BYTEASK_HOME/byok-config.json" "$BYTEASK_HOME/models-catalog.json"
python3 -c "import json;c=json.load(open('$BYTEASK_HOME/models-catalog.json'));import sys;sys.exit(0 if 'self/my-dgx' in [m['slug'] for m in c['models']] else 1)" \
  && ok "custom row restored after update" || bad "update dropped custom row"

echo "== remove of the ACTIVE model resets to a cloud default =="
run models add active-one --url http://x:8000 --model M --wire chat >/dev/null 2>&1
# make it the active model
sed -i.bak 's|^model = .*|model = "self/active-one"|' "$BYTEASK_HOME/config.toml"; rm -f "$BYTEASK_HOME/config.toml.bak"
run models remove active-one >/dev/null 2>&1
grep -q '^model = "auto"' "$BYTEASK_HOME/config.toml" \
  && ok "active-model remove reset to auto (the default)" || bad "active-model remove"
run models list 2>/dev/null | grep -q "self/active-one" && bad "removed alias still listed" || ok "removed alias gone"

echo "== fail-closed help / usage =="
run models bogus 2>&1 | grep -q "usage: byteask models" && ok "unknown subcommand -> usage" || bad "usage"

echo "== provider presets =="
# A stub OpenAI-compatible server so --provider (with --url override) needs no real cloud.
cat > "$TMP/stub.py" <<'PY'
import http.server, json, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.endswith('/v1/models'):
            b = json.dumps({"data": [{"id": "M1"}, {"id": "M2"}, {"id": "M3"}, {"id": "M4"}]}).encode()
            self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers()
    def log_message(self, *a): pass
srv = http.server.HTTPServer(("127.0.0.1", 0), H)
open(sys.argv[1], "w").write("http://127.0.0.1:%d" % srv.server_address[1])
srv.serve_forever()
PY
python3 "$TMP/stub.py" "$TMP/stub.url" & STUB_PID=$!
trap 'kill "$STUB_PID" 2>/dev/null || true; rm -rf "$TMP"' EXIT
i=0; while [ ! -s "$TMP/stub.url" ] && [ "$i" -lt 20 ]; do sleep 0.2; i=$((i+1)); done
STUB="$(cat "$TMP/stub.url" 2>/dev/null)"

run models providers 2>/dev/null | grep -q "openrouter" && ok "providers lists openrouter (cloud)" || bad "providers cloud"
run models providers 2>/dev/null | grep -q "ollama" && ok "providers lists ollama (local)" || bad "providers local"

# --provider fills the preset; explicit --url overrides it; provider_id stored.
run models add op --provider openrouter --url "$STUB" --model M1 --key sk-argv >/dev/null 2>&1 \
  && ok "add --provider openrouter succeeds" || bad "add --provider"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op provider_id)" = openrouter ] \
  && ok "provider_id stored" || bad "provider_id"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op base_url)" = "$STUB" ] \
  && ok "explicit --url overrode preset url" || bad "url override"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op wire)" = chat ] \
  && ok "cloud preset pinned wire=chat (no probe)" || bad "wire pin"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op api_key)" = sk-argv ] \
  && ok "--key stored" || bad "key store"

# honest cloud copy
run models add op-copy --provider openrouter --url "$STUB" --model M1 --key sk 2>&1 | grep -q "they bill you" \
  && ok "cloud add prints honest billing copy" || bad "honest copy"

# BK_KEY: the out-of-argv channel the engine's in-TUI form uses. It stages the key in an
# owner-only file (a marker is world-readable) and the wrapper feeds it in via the env, so
# the key never appears in /proc/<pid>/cmdline.
BK_KEY=sk-bkenv run models add op-bk --provider openrouter --url "$STUB" --model M3 >/dev/null 2>&1 \
  && ok "add reads key from \$BK_KEY (in-TUI form channel)" || bad "BK_KEY add"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op-bk api_key)" = sk-bkenv ] \
  && ok "BK_KEY stored" || bad "BK_KEY store"
# An explicit --key still wins, so the CLI is never silently overridden by a stray env var.
BK_KEY=sk-bkenv run models add op-bk2 --provider openrouter --url "$STUB" --model M3 --key sk-explicit >/dev/null 2>&1
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op-bk2 api_key)" = sk-explicit ] \
  && ok "--key beats \$BK_KEY" || bad "--key precedence"

# key from the provider env var (snapshot)
OPENROUTER_API_KEY=sk-env run models add op-env --provider openrouter --url "$STUB" --model M2 >/dev/null 2>&1 \
  && ok "add reads key from \$OPENROUTER_API_KEY" || bad "env key add"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" op-env api_key)" = sk-env ] \
  && ok "env key stored (snapshot)" || bad "env key store"

# non-TTY cloud preset without --model -> hard error naming --model + example ids
if run models add op-nomodel --provider openrouter --url "$STUB" --key sk 2>&1 | grep -q "pass --model"; then
  ok "non-TTY cloud preset without --model errors (no silent first-id)"
else bad "non-TTY guard"; fi

# unknown provider -> clear error, not a crash
if run models add nope --provider not-a-provider --key sk 2>&1 | grep -q "unknown provider"; then
  ok "unknown --provider errors cleanly"
else bad "unknown provider"; fi

# --ollama aliases to --provider ollama (provider_id recorded) when the helper supports presets
run models add oll --ollama --url "$STUB" --model M1 >/dev/null 2>&1 \
  && ok "--ollama add succeeds" || bad "--ollama add"
[ "$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" oll provider_id)" = ollama ] \
  && ok "--ollama recorded provider_id=ollama" || bad "--ollama alias"

kill "$STUB_PID" 2>/dev/null || true

# ---- live Ollama (optional) ----
if curl -fsS -m 2 http://127.0.0.1:11434/api/version >/dev/null 2>&1; then
  echo "== live Ollama: discovery + probe + test =="
  MODEL="$(curl -fsS http://127.0.0.1:11434/v1/models 2>/dev/null | python3 -c 'import sys,json; d=json.load(sys.stdin).get("data") or []; print(d[0]["id"] if d else "")' 2>/dev/null)"
  if [ -n "$MODEL" ]; then
    run models add ollama-live --url http://127.0.0.1:11434 --model "$MODEL" >/dev/null 2>&1 \
      && ok "add against live Ollama (wire auto-probed)" || bad "add live"
    W="$(python3 "$GW/byteask_models.py" get "$BYTEASK_HOME/byok-config.json" ollama-live wire)"
    case "$W" in responses|chat) ok "wire probed -> $W";; *) bad "wire probe ($W)";; esac
    if run models test ollama-live 2>/dev/null | grep -q "reachable"; then ok "models test ran"; else bad "models test"; fi
  else
    echo "  (Ollama up but no models pulled — skipping live add/test)"
  fi
else
  echo "  (no local Ollama — skipping live discovery/probe/test)"
fi

echo "== launch warning for a model that failed the tool round-trip =="
# A self/* model that can't emit structured tool calls answers prose but silently
# no-ops every edit/shell/search. `models test` records tools_ok; the launch path
# must repeat that verdict, and must stay quiet for every other case.
run models add badtool  --url http://x.lan:8000 --model tiny-3b >/dev/null 2>&1
run models add goodtool --url http://x.lan:8000 --model big-70b >/dev/null 2>&1
python3 -c "
import json,sys
p=sys.argv[1]; c=json.load(open(p))
c['endpoints']['badtool']['tools_ok']=False
c['endpoints']['goodtool']['tools_ok']=True
json.dump(c,open(p,'w'))
" "$BYTEASK_HOME/byok-config.json"

warn_for() {   # $1 = model slug -> echoes how many warning lines were printed
  sed -i.bak 's|^model = .*|model = "'"$1"'"|' "$BYTEASK_HOME/config.toml"
  BYTEASK_HOME="$BYTEASK_HOME" BYTEASK_SOURCE_ONLY=1 sh -c '. "$0"; _warn_self_tools' "$WRAPPER" 2>&1 \
    | grep -c "failed the tool round-trip" || true
}
[ "$(warn_for self/badtool)"  = 1 ] && ok "warns on a self model that failed the tool check" || bad "no warning for a known-bad self model"
[ "$(warn_for self/goodtool)" = 0 ] && ok "silent for a self model that passed"              || bad "warned for a good self model"
[ "$(warn_for self/my-dgx)"   = 0 ] && ok "silent for a self model never tested"             || bad "warned for an untested self model"
[ "$(warn_for gpt-5.4)"       = 0 ] && ok "silent for a managed model"                       || bad "warned for a managed model"
# Fail-open: a missing helper must never block or nag at launch.
mv "$BYTEASK_HOME/byteask_models.py" "$BYTEASK_HOME/_hidden.py"
[ "$(warn_for self/badtool)" = 0 ] && ok "fails open when the helper is missing" || bad "not fail-open"
mv "$BYTEASK_HOME/_hidden.py" "$BYTEASK_HOME/byteask_models.py"

echo ""
echo "models.sh: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
