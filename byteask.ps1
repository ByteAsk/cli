#!/usr/bin/env pwsh
# byteask.ps1 - ByteAsk AI coding agent CLI (native Windows launcher).
#
# PowerShell port of the POSIX `byteask` wrapper: same auth flow (magic-link, mode-C
# token in config.toml), same hourly re-prompting update check, same interactive
# onboarding, and the same /login|/logout launch loop (the TUI drops a marker; we
# re-auth here and relaunch, since the engine can't hot-swap the startup-loaded token).
#
# Behavior parity with byteask/cli/byteask is intentional - keep them in sync.

$VERSION = '0.1.12'
$DEFAULT_GATEWAY = 'https://code.byteask.ai'

$CODEX_HOME = if ($env:BYTEASK_HOME) { $env:BYTEASK_HOME } else { Join-Path $HOME '.byteask' }
$env:CODEX_HOME = $CODEX_HOME
$env:CODEX_BRAND = if ($env:BYTEASK_BRAND) { $env:BYTEASK_BRAND } else { 'ByteAsk' }
$env:BYTEASK_CLIENT_VERSION = $VERSION       # engine displays THIS, not its crate version

$SELF_DIR = $PSScriptRoot
$ENGINE = Join-Path $SELF_DIR 'byteask-engine.exe'
$UPDATE_STATE = Join-Path $CODEX_HOME 'update-check'
$AUTH_REQ = Join-Path $CODEX_HOME '.byteask-auth-request'

function Write-Err([string]$m) { [Console]::Error.WriteLine($m) }   # stderr, non-throwing

function Resolve-Gateway {
  if ($env:BYTEASK_GATEWAY) { return $env:BYTEASK_GATEWAY }
  $f = Join-Path $CODEX_HOME 'gateway'
  if (Test-Path $f) { return ((Get-Content -Raw $f).Trim()) }
  return $DEFAULT_GATEWAY
}

# True only when both stdin and stdout are a real console (mirrors `[ -t 0 ] && [ -t 1 ]`).
function Test-Interactive {
  return (-not [Console]::IsInputRedirected) -and (-not [Console]::IsOutputRedirected)
}

# $true iff A > B, numeric dot-separated, tolerant of junk (matches version_gt).
function Test-VersionGt([string]$a, [string]$b) {
  if ($a -eq $b) { return $false }
  $pa = $a -split '\.'; $pb = $b -split '\.'
  $n = [Math]::Max($pa.Count, $pb.Count)
  for ($i = 0; $i -lt $n; $i++) {
    $ia = 0; $ib = 0
    if ($i -lt $pa.Count) { [void][int]::TryParse($pa[$i], [ref]$ia) }
    if ($i -lt $pb.Count) { [void][int]::TryParse($pb[$i], [ref]$ib) }
    if ($ia -gt $ib) { return $true }
    if ($ia -lt $ib) { return $false }
  }
  return $false
}

# Rewrite config.toml without the token line; clear auth.json. Returns 0 (ok / not
# signed in) or 1 (token present but not removable). Writes messages to host/stderr.
# Full logout across ALL auth stores (managed token, BYOK keys+JWT, subscription auth.json)
# + stop the sidecar + reset to a managed UNSIGNED config. Mirrors the sh do_logout.
function Invoke-Logout {
  $wasSigned = Test-SignedIn
  # 1. stop the BYOK sidecar (holds keys + JWT in memory)
  $pidf = Join-Path $CODEX_HOME 'byok-sidecar.pid'
  if (Test-Path $pidf) { try { Stop-Process -Id ([int](Get-Content $pidf)) -ErrorAction SilentlyContinue } catch {}; Remove-Item -Force -ErrorAction SilentlyContinue $pidf }
  # 2. clear every credential store
  Remove-Item -Force -ErrorAction SilentlyContinue $BYOK_CFG
  Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $CODEX_HOME 'auth.json')
  # 3. reset to managed + UNSIGNED
  $model = Get-CfgLine '^model = "(.*)"$'
  if (-not $model) { $model = if ($env:BYTEASK_MODEL) { $env:BYTEASK_MODEL } else { 'gpt-5.4' } }
  $gw = (Resolve-Gateway).TrimEnd('/')
  Write-ManagedConfig $model (Get-CfgLine '^(model_catalog_json = .*)$') $gw '' | Out-Null
  # 4. report on actual post-state
  if (Test-SignedIn) { Write-Err "Couldn't fully log out - check permissions on $CODEX_HOME"; return 1 }
  if ($wasSigned) { Write-Host "Logged out of ByteAsk. Run 'byteask' to sign back in." }
  else { Write-Host "You're not signed in to ByteAsk." }
  return 0
}

# Magic-link sign-in; writes gateway + config.toml with the mode-C token. Exits 1 on
# failure (like the sh wrapper). $Email/$Ref empty = prompt / discover.
# ===================== BYOK (bring your own key) - Windows parity ===========
# Mirrors the sh wrapper's byok commands. Keys + the managed JWT live in
# ~/.byteask/byok-config.json (0600 best-effort); a local python "sidecar" reads it
# and the engine points at it, routing per model (own-key -> provider direct; no key
# -> managed gateway). Requires python. KEEP THIS FILE PURE ASCII (PS 5.1 codepage).
$BYOK_PORT = if ($env:BYOK_SIDECAR_PORT) { $env:BYOK_SIDECAR_PORT } else { '8799' }
$BYOK_CFG = Join-Path $CODEX_HOME 'byok-config.json'
$BYOK_SIDECAR = Join-Path $CODEX_HOME 'byok_sidecar.py'
$BYOK_MODELS = Join-Path $CODEX_HOME 'byteask_models.py'   # shared self-hosted-models helper

function Get-ModelsPython {
  foreach ($p in @('python3','python')) { if (Get-Command $p -ErrorAction SilentlyContinue) { return $p } }
  return $null
}
function Test-ModelsReady {
  if (-not (Test-Path $BYOK_MODELS)) { Write-Err "byteask: self-hosted models need the helper; run 'byteask --update'."; return $false }
  if (-not (Get-ModelsPython)) { Write-Err "byteask: self-hosted models need python3 (not found on PATH)."; return $false }
  return $true
}
function Invoke-ModelsPy([string[]]$PyArgs) {
  $py = Get-ModelsPython; if (-not $py) { return @() }
  try { return @(& $py $BYOK_MODELS @PyArgs 2>$null) } catch { return @() }
}
function Read-HiddenLine([string]$prompt) {
  $sec = Read-Host -AsSecureString $prompt
  return [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
}
# Re-merge the registry's self/* endpoints into the catalog (idempotent + atomic; never
# drops a base model). At launch + after add/remove so /model tracks the registry and a
# --update that re-fetched a base catalog can't permanently drop custom rows.
function Invoke-ModelsMerge {
  if (-not (Test-Path $BYOK_MODELS)) { return }
  $py = Get-ModelsPython; if (-not $py) { return }
  $cat = Join-Path $CODEX_HOME 'models-catalog.json'
  if (-not (Test-Path $cat)) { return }
  try { & $py $BYOK_MODELS merge-catalog $BYOK_CFG $cat 2>$null | Out-Null } catch {}
}
function Test-HasSelfEndpoints {
  if (-not (Test-Path $BYOK_CFG)) { return $false }
  return (Select-String -Path $BYOK_CFG -Pattern '"endpoints"' -Quiet)
}

function Get-PyExe {
  foreach ($p in @('python3','python','py')) {
    $c = Get-Command $p -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
  }
  return $null
}

# Run an inline Python snippet by writing it to a temp .py file and executing THAT,
# never via `python -c <code>`. Windows PowerShell 5.1 does not escape embedded
# double-quotes or newlines when it builds the command line for a native command, so
# a multi-line -c snippet with "quoted" dict keys reaches python.exe truncated ->
# SyntaxError: '(' was never closed (Windows 11, 2026-07-16). Routing the code through
# a temp file sidesteps ALL command-line quoting of the code; only simple args (file
# paths, provider names) are passed, which 5.1 quotes correctly. sys.argv matches the
# -c form: argv[0] is the script, argv[1..] are $ExtraArgs. Returns $null on no-python
# or any failure (callers fall back to a default). Mirrors the sh wrapper's `python3 -`.
function Invoke-Py([string]$Code, [string[]]$ExtraArgs) {
  $py = Get-PyExe
  if (-not $py) { return $null }
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('byteask-py-' + [Guid]::NewGuid().ToString('N') + '.py')
  try {
    [IO.File]::WriteAllText($tmp, $Code, (New-Object Text.UTF8Encoding $false))  # no BOM
    if ($ExtraArgs) { return (& $py $tmp @ExtraArgs 2>$null) }
    return (& $py $tmp 2>$null)
  } catch {
    return $null
  } finally {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmp
  }
}

# JSON ops via python (BYOK requires python anyway - no jq/PS-JSON edge cases).
$BYOK_MERGE_PY = @'
import json,sys,os
path=sys.argv[1]
try: cfg=json.load(open(path))
except Exception: cfg={}
if not isinstance(cfg,dict): cfg={}
cfg.setdefault("keys",{})
for pair in sys.argv[2:]:
    k,_,v=pair.partition("=")
    if k.startswith("keys."):
        prov=k[5:]
        if v: cfg["keys"][prov]=v
        else: cfg["keys"].pop(prov,None)
    else:
        if v: cfg[k]=v
        else: cfg.pop(k,None)
os.makedirs(os.path.dirname(path),exist_ok=True)
open(path,"w").write(json.dumps(cfg))
try: os.chmod(path,0o600)
except Exception: pass
'@
$BYOK_FIELD_PY = 'import json,sys' + "`n" + 'try: print(json.load(open(sys.argv[1])).get(sys.argv[2]) or "")' + "`n" + 'except Exception: print("")'
$BYOK_COUNT_PY = 'import json,sys' + "`n" + 'try: print(len((json.load(open(sys.argv[1])).get("keys") or {})))' + "`n" + 'except Exception: print(0)'

function Invoke-ByokMerge([string[]]$pairs) {
  [void](Invoke-Py $BYOK_MERGE_PY (@($BYOK_CFG) + $pairs))
}
function Get-ByokField([string]$name) {
  if (-not (Test-Path $BYOK_CFG)) { return '' }
  $v = Invoke-Py $BYOK_FIELD_PY @($BYOK_CFG, $name)
  if ($null -eq $v) { return '' }
  return $v
}
function Get-ByokKeyCount {
  if (-not (Test-Path $BYOK_CFG)) { return 0 }
  $c = Invoke-Py $BYOK_COUNT_PY @($BYOK_CFG)
  if (-not $c) { return 0 }
  return [int]$c
}
# Per-provider menu verb: "change" if that key is set, else "add" (openai anthropic gemini).
function Get-ByokKeyVerbs {
  if (-not (Test-Path $BYOK_CFG)) { return @('add','add','add') }
  $snippet = 'import json,sys' + "`n" + 'try: keys=json.load(open(sys.argv[1])).get("keys") or {}' + "`n" + 'except Exception: keys={}' + "`n" + 'print(" ".join("change" if keys.get(p) else "add" for p in ("openai","anthropic","gemini")))'
  $out = Invoke-Py $snippet @($BYOK_CFG)
  if (-not $out) { return @('add','add','add') }
  $parts = ($out.Trim() -split '\s+')
  if ($parts.Count -lt 3) { return @('add','add','add') }
  return $parts
}
function Get-CfgLine([string]$pattern) {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  if (-not (Test-Path $cfg)) { return '' }
  $m = Select-String -Path $cfg -Pattern $pattern | Select-Object -First 1
  if ($m) { return $m.Matches[0].Groups[1].Value } else { return '' }
}
function Get-CurrentJwt {
  $j = Get-ByokField 'jwt'
  if ($j) { return $j }
  return (Get-CfgLine '^experimental_bearer_token = "(.*)"$')
}

# Renew the managed session before launching, so an ordinary user never reaches
# `exp`. Silent and fail-open on every path: it never writes a token it did not
# get, and any error leaves the existing config untouched.
function Invoke-SessionRefresh {
  if ($env:BYTEASK_NO_REFRESH -eq '1') { return }
  $prov = Get-CfgLine '^model_provider = "(.*)"$'
  if ($prov -eq 'byteask') { $tok = Get-ManagedToken }
  elseif ($prov -eq 'byok-local') { $tok = Get-ByokField 'jwt' }
  else { return }
  if (-not $tok) { return }
  $left = Get-JwtTtlLeft $tok
  if ($null -eq $left) { return }        # undecodable: do not hammer the gateway
  if ($left -ge 604800) { return }       # renew only inside the last week of life
  $gw = (Resolve-Gateway).TrimEnd('/')
  if (-not $gw) { return }
  try {
    $resp = Invoke-RestMethod -Method Post -Uri "$gw/auth/refresh" -TimeoutSec 8 `
      -Headers @{ Authorization = "Bearer $tok" } -ContentType 'application/json' -Body '{}'
  } catch { return }
  $new = $resp.access_token
  if (-not $new) { return }
  if ($new.Split('.').Count -ne 3) { return }
  if ($prov -eq 'byok-local') {
    try { Invoke-ByokMerge @("jwt=$new") } catch { }
  } else {
    if (-not (Set-ManagedToken $new)) { Write-ConfigWriteFailure }
  }
}

# Emails that have signed in on this machine (persists across logout). Used to
# skip the referral prompt for a returning user - referrals only credit a NEW signup.
function Test-EmailKnown([string]$Email) {
  $ke = Join-Path $CODEX_HOME '.known-emails'
  if (-not (Test-Path $ke)) { return $false }
  $want = $Email.Trim().ToLowerInvariant()
  foreach ($line in (Get-Content $ke -ErrorAction SilentlyContinue)) {
    if ($line.Trim().ToLowerInvariant() -eq $want) { return $true }
  }
  return $false
}
function Add-KnownEmail([string]$Email) {
  if (Test-EmailKnown $Email) { return }
  $ke = Join-Path $CODEX_HOME '.known-emails'
  try { Add-Content -Path $ke -Value $Email -ErrorAction SilentlyContinue } catch { }
}

# Authoritative "has this email signed in before?" via the gateway (works across
# machines, unlike the local cache). Returns 'yes' | 'no' | '' (couldn't tell).
# Fail-open: any error / older gateway without the route -> '' (caller uses local).
function Get-ServerEmailExists([string]$Email) {
  $gw = (Resolve-Gateway).TrimEnd('/')
  if (-not $gw) { return '' }
  try {
    $enc = [uri]::EscapeDataString($Email)   # encodes '+' -> %2B, '@' -> %40
    $r = Invoke-RestMethod -Uri "$gw/auth/account-exists?email=$enc" -TimeoutSec 3 -ErrorAction Stop
    if ($null -ne $r.exists) { if ($r.exists) { return 'yes' } else { return 'no' } }
  } catch { }
  return ''
}
# True (=> SKIP the referral prompt) when the email is a returning user. Server first,
# local .known-emails fallback when the gateway can't be reached.
function Test-EmailReturning([string]$Email) {
  switch (Get-ServerEmailExists $Email) {
    'yes'   { return $true }
    'no'    { return $false }
    default { return (Test-EmailKnown $Email) }
  }
}

# One gateway probe per sign-in attempt. From a single /auth/account-exists call it
# returns @{ Block = <reason the address is refused, or ''>; Exists = 'yes'|'no'|'' }.
# Fail-open: any error leaves both empty -> allow, with /auth/start the authoritative gate.
function Get-EmailProbe([string]$Email) {
  $out = @{ Block = ''; Exists = '' }
  $gw = (Resolve-Gateway).TrimEnd('/')
  if (-not $gw) { return $out }
  try {
    $enc = [uri]::EscapeDataString($Email)
    $r = Invoke-RestMethod -Uri "$gw/auth/account-exists?email=$enc" -TimeoutSec 3 -ErrorAction Stop
    if ($r.blocked) { $out.Block = [string]$r.error; return $out }
    if ($null -ne $r.exists) { $out.Exists = if ($r.exists) { 'yes' } else { 'no' } }
  } catch { }
  return $out
}
# True => this email is a NEW signup (show the referral prompt). Uses the probe's Exists,
# local .known-emails fallback when the server was silent.
function Test-EmailNew([string]$Email, [string]$Exists) {
  switch ($Exists) {
    'yes'   { return $false }
    'no'    { return $true }
    default { return (-not (Test-EmailKnown $Email)) }
  }
}

# Write config.toml through a temp file + Move-Item, then read back what landed.
# Returns $false when the config was NOT persisted; every caller must check.
#
# Two reasons, both learned from the 2026-09-07 lockout on the POSIX side: writing
# in place fails outright when config.toml is owned by another account, and a write
# whose result is never checked turns a failed sign-in into "Signed in as ..."
# followed by an unbreakable 401 loop. A rename needs the DIRECTORY, not the file,
# so this also succeeds in the case that used to fail.
function Save-ConfigToml([string]$content, [string]$expectToken) {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  $tmp = "$cfg.tmp.$PID"
  try {
    New-Item -ItemType Directory -Force -Path $CODEX_HOME -ErrorAction SilentlyContinue | Out-Null
    Set-Content -Path $tmp -Value $content -ErrorAction Stop
    Move-Item -Force -Path $tmp -Destination $cfg -ErrorAction Stop
  } catch {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmp
    Write-ConfigWriteFailure
    return $false
  }
  if ($expectToken) {
    if (-not (Select-String -Path $cfg -Pattern ([regex]::Escape("experimental_bearer_token = `"$expectToken`"")) -Quiet)) {
      Write-ConfigWriteFailure
      return $false
    }
  }
  return $true
}

# Replace ONLY the experimental_bearer_token line, keeping every other line the user
# or the engine has put in config.toml (trusted projects, reasoning effort, MCP
# servers). Write-ManagedConfig regenerates the whole file and is right for a
# sign-in; a renewal must be invisible. Returns $false unless the new token landed.
function Set-ManagedToken([string]$token) {
  if (-not $token) { return $false }
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  if (-not (Test-Path $cfg)) { return $false }
  $lines = @(Get-Content $cfg -ErrorAction SilentlyContinue)
  $hit = $false
  for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^experimental_bearer_token = ') {
      $lines[$i] = "experimental_bearer_token = `"$token`""; $hit = $true
    }
  }
  if (-not $hit) { return $false }
  return (Save-ConfigToml ($lines -join "`r`n") $token)
}

function Write-ConfigWriteFailure {
  Write-Err ""
  Write-Err ("byteask: could not save your sign-in to " + (Join-Path $CODEX_HOME 'config.toml'))
  Write-Err "  Nothing was changed, so this session is still using the old credential."
  Write-Err "  Check that the file is not read-only and that you own it, then run: byteask login"
}

function Write-ManagedConfig([string]$model, [string]$catalog, [string]$gateway, [string]$token) {
  # Empty $token => UNSIGNED (no experimental_bearer_token line) so the launch check onboards.
  $tokenLine = if ($token) { "experimental_bearer_token = `"$token`"" } else { "" }
  $cfg = @"
model = "$model"
model_provider = "byteask"
web_search = "live"
$catalog

[model_providers.byteask]
name = "ByteAsk"
base_url = "$gateway/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
$tokenLine

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
"@
  $cfg = Add-TersePref $cfg
  return (Save-ConfigToml $cfg $token)
}
function Write-ByokConfig([string]$model, [string]$catalog, [string]$token) {
  $cfg = @"
model = "$model"
model_provider = "byok-local"
web_search = "live"
$catalog

[model_providers.byok-local]
name = "ByteAsk (your key)"
base_url = "http://127.0.0.1:$BYOK_PORT/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[model_providers.byok-local.http_headers]
X-BYOK-Token = "$token"
x-openai-actor-authorization = "byteask"
"@
  $cfg = Add-TersePref $cfg
  return (Save-ConfigToml $cfg '')
}

# Terse mode: gateway-injected output-style floor (default-on lite). The level
# rides an x-byteask-terse header the gateway reads to append a style block to
# `instructions`. Persisted in CODEX_HOME/terse so it survives re-login. Parity
# with the sh wrapper's do_terse. Pure ASCII (PS 5.1 codepage rule).
function Add-TersePref([string]$cfg) {
  # Re-emit the saved terse level when (re)writing config, so a re-login keeps it.
  $pref = Join-Path $CODEX_HOME 'terse'
  if (Test-Path $pref) {
    $lvl = (Get-Content -Raw $pref -ErrorAction SilentlyContinue)
    if ($lvl) { $lvl = $lvl.Trim() }
    if ($lvl) { $cfg = $cfg + "`nx-byteask-terse = `"$lvl`"" }
  }
  return $cfg
}
function Get-TerseLevelNow {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  if (-not (Test-Path $cfg)) { return '' }
  $m = Select-String -Path $cfg -Pattern '^x-byteask-terse = "(.*)"$' | Select-Object -First 1
  if ($m) { return $m.Matches[0].Groups[1].Value }
  return ''
}
function Set-TerseConfig([string]$level) {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  if (-not (Test-Path $cfg)) { return }
  $out = New-Object System.Collections.Generic.List[string]
  $inHdrs = $false
  foreach ($ln in (Get-Content -Path $cfg)) {
    $s = $ln.Trim()
    if ($s.StartsWith('[') -and $s.EndsWith(']')) {
      $inHdrs = $s.EndsWith('.http_headers]')
      $out.Add($ln)
      if ($inHdrs) { $out.Add("x-byteask-terse = `"$level`"") }
      continue
    }
    if ($inHdrs -and $s.ToLower().StartsWith('x-byteask-terse')) { continue }
    $out.Add($ln)
  }
  Set-Content -Path $cfg -Value $out
}
function Invoke-Terse([string[]]$rest) {
  $arg = if ($rest.Count -ge 1) { "$($rest[0])" } else { 'status' }
  switch -Regex ($arg) {
    '^(status)?$' {
      $tl = Get-TerseLevelNow; if (-not $tl) { $tl = 'lite (default)' }
      Write-Host "Terse mode: $tl"
      Write-Host "  concise replies, code/commands/errors kept exact. Change:"
      Write-Host "  byteask terse off | lite | full | ultra"
      return 0
    }
    '^(off|lite|full|ultra)$' { }
    '^(-h|--help)$' { Write-Host "usage: byteask terse [status|off|lite|full|ultra]"; return 0 }
    default { Write-Err "byteask terse: unknown level '$arg' (use off|lite|full|ultra|status)"; return 2 }
  }
  if (-not (Test-Path $CODEX_HOME)) { New-Item -ItemType Directory -Force -Path $CODEX_HOME | Out-Null }
  Set-Content -Path (Join-Path $CODEX_HOME 'terse') -Value $arg -NoNewline
  Set-TerseConfig $arg
  switch ($arg) {
    'off'   { Write-Host "Terse mode OFF - replies use the model's normal style." }
    'lite'  { Write-Host "Terse mode LITE (default) - concise; skips filler, keeps all code/technical detail exact." }
    'full'  { Write-Host "Terse mode FULL - tight, fragment-style replies; code/commands/errors kept verbatim." }
    'ultra' { Write-Host "Terse mode ULTRA - maximum terseness; code/commands/errors kept verbatim." }
  }
  Write-Host "  Takes effect on your next 'byteask' launch."
  return 0
}

function Test-SidecarHealth {
  try { $null = Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$BYOK_PORT/healthz"; return $true }
  catch { return $false }
}
function Ensure-Sidecar {
  if ($env:BYOK_SKIP_SIDECAR) { return $true }
  if (-not (Test-Path $BYOK_SIDECAR)) { Write-Err "byteask: BYOK sidecar not installed; run 'byteask --update'."; return $false }
  $pidf = Join-Path $CODEX_HOME 'byok-sidecar.pid'
  if (Test-SidecarHealth) {
    if ((Test-Path $pidf) -and ((Get-Item $pidf).LastWriteTime -gt (Get-Item $BYOK_SIDECAR).LastWriteTime)) { return $true }
    try { Stop-Process -Id ([int](Get-Content $pidf -ErrorAction SilentlyContinue)) -ErrorAction SilentlyContinue } catch {}
    Start-Sleep -Milliseconds 500
  }
  $py = Get-PyExe
  if (-not $py) { Write-Err "byteask: BYOK needs python (not found on PATH)."; return $false }
  $env:BYOK_SIDECAR_PORT = $BYOK_PORT
  $p = Start-Process -FilePath $py -ArgumentList $BYOK_SIDECAR -WindowStyle Hidden -PassThru
  Set-Content -Path $pidf -Value $p.Id -NoNewline
  for ($i = 0; $i -lt 30; $i++) { if (Test-SidecarHealth) { return $true }; Start-Sleep -Milliseconds 100 }
  Write-Err "byteask: BYOK sidecar didn't start."; return $false
}

function Test-ProviderKey([string]$prov, [string]$key) {
  switch ($prov) {
    'openai'    { $u = 'https://api.openai.com/v1/models'; $h = @{ 'Authorization' = "Bearer $key" } }
    'anthropic' { $u = 'https://api.anthropic.com/v1/models'; $h = @{ 'x-api-key' = $key; 'anthropic-version' = '2023-06-01' } }
    'gemini'    { $u = 'https://generativelanguage.googleapis.com/v1beta/models'; $h = @{ 'x-goog-api-key' = $key } }
    default { return $true }
  }
  try { $null = Invoke-WebRequest -UseBasicParsing -TimeoutSec 15 -Headers $h $u; return $true }
  catch {
    $code = 0; try { $code = [int]$_.Exception.Response.StatusCode } catch {}
    if ($code -eq 401 -or $code -eq 403) { Write-Err "  That $prov key was rejected by the provider (HTTP $code)."; return $false }
    Write-Err "  Couldn't verify the $prov key right now (HTTP $code) - saving it anyway."; return $true
  }
}

function Invoke-ByokEnter {
  $jwt = Get-CurrentJwt
  $lt = Get-ByokField 'local_token'
  if (-not $lt) { $lt = Invoke-Py 'import secrets;print(secrets.token_hex(24))' }
  $gw = (Resolve-Gateway).TrimEnd('/')
  Invoke-ByokMerge @("jwt=$jwt", "local_token=$lt", "gateway=$gw")
  if (-not (Ensure-Sidecar)) { return $false }
  if (-not (Write-ByokConfig (Get-CfgLine '^model = "(.*)"$') (Get-CfgLine '^(model_catalog_json = .*)$') $lt)) { return $false }
  return $true
}

# Prompt (masked) + validate + store + activate ONE provider key. Returns $true/$false
# (no exit) so the source menu can loop. Reused by `byok set` (CLI) + the menu.
function Add-ByokKey([string]$prov) {
  if (-not (Test-Path $BYOK_SIDECAR)) { Write-Err "byteask: BYOK needs the sidecar; run 'byteask --update' first."; return $false }
  if (-not (Get-PyExe)) { Write-Err "byteask: BYOK needs python (not found)."; return $false }
  $sec = Read-Host -AsSecureString "Paste your $prov API key (hidden, never shown)"
  $key = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
  if (-not $key) { Write-Err "No key entered."; return $false }
  if (-not (Test-ProviderKey $prov $key)) { return $false }
  Invoke-ByokMerge @("keys.$prov=$key")
  if (-not (Invoke-ByokEnter)) { return $false }
  Write-Host "Saved your $prov key."
  return $true
}

# One-line per-provider state for the source menu + status.
function Get-ByokStatusLine {
  $code = @'
import json,sys
try: keys=json.load(open(sys.argv[1])).get("keys") or {}
except Exception: keys={}
lbl={"openai":"OpenAI","anthropic":"Anthropic","gemini":"Gemini"}
print("  ".join("%s=%s"%(lbl[p],"your key" if keys.get(p) else "managed") for p in ("openai","anthropic","gemini")))
'@
  $out = Invoke-Py $code @($BYOK_CFG)
  if ($out) { return $out }
  return "OpenAI=managed  Anthropic=managed  Gemini=managed"
}

# Signed in iff a JWT exists AND (best-effort) is not expired (exp claim, decoded read-only).
# The managed credential and ONLY that: the experimental_bearer_token line is
# literally what the engine puts in the Authorization header.
function Get-ManagedToken {
  return (Get-CfgLine '^experimental_bearer_token = "(.*)"$')
}

# Seconds until this JWT expires; $null when it cannot be decoded, so callers can
# tell "expired" apart from "unknown" instead of collapsing both into one boolean.
function Get-JwtTtlLeft([string]$jwt) {
  if (-not $jwt) { return $null }
  try {
    $seg = $jwt.Split('.')[1].Replace('-','+').Replace('_','/')
    switch ($seg.Length % 4) { 2 { $seg += '==' } 3 { $seg += '=' } }
    $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($seg))
    $exp = [regex]::Match($json, '"exp"\s*:\s*([0-9]+)').Groups[1].Value
    if (-not $exp) { return $null }
    return ([long]$exp - [DateTimeOffset]::UtcNow.ToUnixTimeSeconds())
  } catch { return $null }
}

function Test-SignedIn {
  # On the managed provider the engine sends config.toml's token and nothing else,
  # so that is the token whose expiry decides whether this machine has a session.
  # Get-CurrentJwt reads byok-config.json FIRST, so a leftover BYOK jwt made an
  # expired managed token report "signed in" forever (2026-09-07).
  if ((Get-CfgLine '^model_provider = "(.*)"$') -eq 'byteask') {
    $jwt = Get-ManagedToken
  } else {
    $jwt = Get-CurrentJwt
  }
  if (-not $jwt) { return $false }
  try {
    $seg = $jwt.Split('.')[1].Replace('-','+').Replace('_','/')
    switch ($seg.Length % 4) { 2 { $seg += '==' } 3 { $seg += '=' } }
    $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($seg))
    $exp = [regex]::Match($json, '"exp"\s*:\s*([0-9]+)').Groups[1].Value
    if ($exp -and [long]$exp -lt [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()) { return $false }
  } catch {}
  return $true
}
# The MANAGED provider authenticates with experimental_bearer_token in config.toml - that
# line IS the credential the engine sends. Get-CurrentJwt reads byok-config.json FIRST, so a
# managed config whose token line is missing still reports "signed in" (and the settings
# screen shows the email) while every turn 401s with "Sign in to continue - type /login".
# True == that broken state.
function Test-ManagedMissingToken {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  if (-not (Test-Path $cfg)) { return $false }
  $pm = Select-String -Path $cfg -Pattern '^model_provider = "(.*)"$' | Select-Object -First 1
  if (-not $pm -or $pm.Matches[0].Groups[1].Value -ne 'byteask') { return $false }
  return (-not (Select-String -Path $cfg -Pattern '^experimental_bearer_token = ' -Quiet))
}

# $true when the ACTIVE config needs a ByteAsk account; $false when fully self-served.
# Mirrors the sh _needs_byteask_signin: managed provider -> needs us; byok-local self/*
# or a cloud model with the user's own key -> direct, no account; byok-local auto or an
# un-keyed cloud model -> managed-forwards, needs us; other providers -> engine owns auth.
function Test-NeedsSignin {
  $prov = Get-CfgLine '^model_provider = "(.*)"$'
  if ($prov -eq 'byteask') { return $true }
  if ($prov -ne 'byok-local') { return $false }
  $model = Get-CfgLine '^model = "(.*)"$'
  if ($model -like 'self/*') { return $false }
  if (-not $model -or $model -eq 'auto') { return $true }
  # Cloud model: no account IFF the user holds their own key for its provider. Reuse the
  # served translator predicates (via a temp .py) so the provider map can't drift; any
  # failure -> conservative "needs us" (never a silent launch that would 401 first turn).
  $code = @'
import json, sys, os
model = os.environ.get("BYTEASK_MODEL", "")
sys.path.insert(0, sys.argv[2])
try:
    import anthropic_translate, gemini_translate
    if anthropic_translate.is_anthropic_model(model):  prov = "anthropic"
    elif gemini_translate.is_gemini_model(model):      prov = "gemini"
    else:                                              prov = "openai"
    keys = json.load(open(sys.argv[1])).get("keys") or {}
    print("direct" if (keys.get(prov) or "").strip() else "managed")
except Exception:
    print("managed")
'@
  $env:BYTEASK_MODEL = $model
  $out = Invoke-Py $code @($BYOK_CFG, $CODEX_HOME)
  Remove-Item Env:\BYTEASK_MODEL -ErrorAction SilentlyContinue
  if ($out -and ($out -join '').Trim() -eq 'direct') { return $false }
  return $true
}
# Interactive iff a real console, or BYOK_ASSUME_TTY set (test seam).
function Test-MenuTty { if ($env:BYOK_ASSUME_TTY) { return $true }; return (Test-Interactive) }

function Invoke-ByokSet([string[]]$rest) {
  $prov = $rest[0]
  if (@('openai','anthropic','gemini') -notcontains $prov) {
    Write-Err "usage: byteask byok set <openai|anthropic|gemini> [--subscription]"; exit 2 }
  if ($prov -eq 'openai' -and (($rest -contains '--subscription') -or ($rest -contains '--sub'))) {
    Invoke-ByokSubscription; return }
  if (-not (Add-ByokKey $prov)) { exit 1 }
  Write-Host "Keyed providers bill to your account; other models use ByteAsk managed (counts toward your usage)."
  Write-Host "Relaunching..."
}

function Show-ByokStatus {
  if ((Get-ByokKeyCount) -eq 0) { Write-Host "BYOK: off (all traffic is managed)."; return }
  Write-Host "BYOK: on. Keyed providers (billed to you):"
  $code = @'
import json,sys
try: keys=json.load(open(sys.argv[1])).get("keys") or {}
except Exception: keys={}
for p in ("openai","anthropic","gemini"): print("  - %s: %s"%(p,"your key" if keys.get(p) else "managed"))
'@
  $lines = Invoke-Py $code @($BYOK_CFG)
  if ($lines) { foreach ($ln in @($lines)) { Write-Host $ln } }
  Write-Host "  Un-keyed providers use ByteAsk managed (billed to you, counts toward your usage)."
  if (Test-SidecarHealth) { Write-Host "  sidecar: running on 127.0.0.1:$BYOK_PORT" } else { Write-Host "  sidecar: not running (starts on next launch)" }
}

function Invoke-ByokRemove([string[]]$rest) {
  $prov = $rest[0]
  if (@('openai','anthropic','gemini') -notcontains $prov) { Write-Err "usage: byteask byok remove <openai|anthropic|gemini>"; exit 2 }
  Invoke-ByokMerge @("keys.$prov=")
  if ((Get-ByokKeyCount) -eq 0) { Invoke-ByokOff } else { [void](Invoke-ByokEnter); Write-Host "Removed your $prov key." }
}

function Invoke-ByokOff {
  $jwt = Get-CurrentJwt; $gw = (Resolve-Gateway).TrimEnd('/')
  $hadKeys = (Get-ByokKeyCount) -ne 0
  $pidf = Join-Path $CODEX_HOME 'byok-sidecar.pid'
  if (Test-Path $pidf) { try { Stop-Process -Id ([int](Get-Content $pidf)) -ErrorAction SilentlyContinue } catch {}; Remove-Item -Force -ErrorAction SilentlyContinue $pidf }
  Invoke-ByokMerge @('keys.openai=','keys.anthropic=','keys.gemini=')
  # A self/* model needs the sidecar; on managed it would fail every turn - reset
  # to the default cloud model (mirrors Remove-Model's active-model handling).
  $offModel = Get-CfgLine '^model = "(.*)"$'
  if ((-not $offModel) -or $offModel.StartsWith('self/')) {
    $offModel = if ($env:BYTEASK_MODEL) { $env:BYTEASK_MODEL } else { 'gpt-5.4' }
  }
  if (-not (Write-ManagedConfig $offModel (Get-CfgLine '^(model_catalog_json = .*)$') $gw $jwt)) {
    Write-Err "byteask: still on your own key - the managed config could not be written."
    return
  }
  if ($hadKeys) { Write-Host "Switched to ByteAsk managed (billed to ByteAsk, /usage as normal)." }
  else { Write-Host "You're on ByteAsk managed (billed to ByteAsk, /usage as normal)." }
}

function Invoke-ByokSubscription {
  Write-Host "Sign in with your ChatGPT subscription (Plus/Pro/Business)."
  Write-Host "Note: OpenAI's own sign-in screen appears; Anthropic/Gemini keys don't mix into a subscription session."
  & $ENGINE login; if ($LASTEXITCODE -ne 0) { Write-Err "ChatGPT sign-in failed."; exit 1 }
  $cfg = @"
model = "$(Get-CfgLine '^model = "(.*)"$')"
model_provider = "openai"
web_search = "live"
$(Get-CfgLine '^(model_catalog_json = .*)$')
"@
  Set-Content -Path (Join-Path $CODEX_HOME 'config.toml') -Value $cfg
  Write-Host "ChatGPT subscription active (OpenAI-only session). 'byteask byok off' to return to managed."
}

function Invoke-Byok([string[]]$rest) {
  $sub = if ($rest.Count -ge 1) { $rest[0] } else { '' }
  $tail = @($rest | Select-Object -Skip 1)
  switch ($sub) {
    'set'    { Invoke-ByokSet $tail }
    'remove' { Invoke-ByokRemove $tail }
    'rm'     { Invoke-ByokRemove $tail }
    'off'    { Invoke-ByokOff }
    { $_ -eq 'status' -or $_ -eq '' } { Show-ByokStatus }
    default  { Write-Err "usage: byteask byok <set|status|remove|off> [provider]"; exit 2 }
  }
}

# ===================== self-hosted / custom models (byteask models) =========
# Point ByteAsk at the user's OWN OpenAI-compatible server (vLLM/TGI/Ollama/LM Studio/
# DGX). Registry + routing + slim catalog row all via the shared python helper, so this
# stays a thin mirror of the sh wrapper. Pure ASCII (PS 5.1 codepage rule).
function Show-SelfHostedHowto {
  Write-Host ""
  Write-Host "Other providers & your own hosted model"
  Write-Host "  A cloud provider with your own key (OpenRouter, Groq, DeepSeek, ...):"
  Write-Host "    byteask models add or --provider openrouter --key sk-...   (they bill you)"
  Write-Host "  Your OWN OpenAI-compatible server (vLLM/TGI/Ollama/LM Studio/DGX):"
  Write-Host "    byteask models add my-model --url http://your-host:8000    (never billed)"
  Write-Host "  See providers:  byteask models providers"
  Write-Host ""
}
function Show-ModelsHelp {
  Write-Err "Use a cloud provider with YOUR key (OpenRouter/Groq/DeepSeek/... - the provider bills you):"
  Write-Err "  byteask models add <alias> --provider <id> [--key K] [--model ID]"
  Write-Err "  byteask models providers                       list the known providers"
  Write-Err "Use your OWN hosted model (vLLM/TGI/Ollama/LM Studio/DGX - never billed by ByteAsk):"
  Write-Err "  byteask models add <alias> --url http://host:8000 [--model ID] [--key K] [--ollama]"
  Write-Err "                             [--wire auto|responses|chat] [--ctx N] [--ca-bundle P] [--insecure]"
  Write-Err "  byteask models list [--check]"
  Write-Err "  byteask models test <alias>"
  Write-Err "  byteask models remove <alias>"
  Write-Err "Then pick self/<alias> in /model."
}
# True (silently) when the shared helper is new enough for preset subcommands (D13).
function Test-PresetHelper {
  $v = (Invoke-ModelsPy @('version'))
  if (-not $v -or $v.Count -lt 1) { return $false }
  $n = 0; if ([int]::TryParse("$($v[0])".Trim(), [ref]$n)) { return ($n -ge 2) }
  return $false
}
function Require-PresetHelper {
  if (Test-PresetHelper) { return $true }
  Write-Err "byteask: provider presets need a newer helper - run 'byteask --update'."
  return $false
}
# Resolve a preset id -> hashtable, or $null if unknown.
function Get-ProviderPreset([string]$id) {
  $line = (Invoke-ModelsPy @('preset', $id))
  if (-not $line -or $line.Count -lt 1) { return $null }
  $parts = "$($line[0])".Split('|')
  if ($parts.Count -lt 6) { return $null }
  return @{ url = $parts[1]; wire = $parts[2]; needs = ($parts[3] -eq '1'); env = $parts[4]; kind = $parts[5] }
}
# Discover model ids; key rides BK_KEY (env, not argv - 3A). On failure maps the
# helper's HTTP status to a tailored hint (4A). Returns the id array (empty on fail).
function Invoke-Discover([string]$url, [string]$key, [string]$ca, [string]$insArg) {
  $py = Get-ModelsPython
  $errf = [System.IO.Path]::GetTempFileName()
  $prev = $env:BK_KEY; $env:BK_KEY = $key
  try { $out = @(& $py $BYOK_MODELS discover $url '' $ca $insArg 2>$errf) }
  finally { if ($null -eq $prev) { Remove-Item Env:BK_KEY -ErrorAction SilentlyContinue } else { $env:BK_KEY = $prev } }
  $rc = $LASTEXITCODE
  $err = (Get-Content $errf -Raw -ErrorAction SilentlyContinue); Remove-Item $errf -ErrorAction SilentlyContinue
  if ($rc -ne 0 -or -not $out -or $out.Count -eq 0) {
    if ($err -match 'HTTP 401' -or $err -match 'HTTP 403') { Write-Err "  the server rejected your key - check --key or your API key." }
    elseif ($err -match 'HTTP 402') { Write-Err "  your account needs credits before it can be used (402)." }
    elseif ($err -match 'HTTP 404') { Write-Err "  no /v1/models at $url - check the URL, or pass --model <id>." }
    elseif ($err -match 'HTTP ')    { Write-Err "  couldn't list models - pass --model <id>." }
    else                            { Write-Err "  couldn't reach $url - check the URL / network." }
    return @()
  }
  return $out
}
# Interactive picker (1B): shows up to 20 ids; a typed substring filters the fetched
# set in place; a number picks the shown slice; an exact id is accepted. Returns the id.
function Select-FromIds([string[]]$ids) {
  $cur = @($ids)
  while ($true) {
    $shown = @($cur | Select-Object -First 20)
    for ($k=0; $k -lt $shown.Count; $k++) { Write-Err "    $($k+1)) $($shown[$k])" }
    if ($cur.Count -gt 20) { Write-Err "    ... $($cur.Count - 20) more - type a substring to filter, or the exact id" }
    $in = Read-Host "  pick a number, or type a substring/exact id [1]"
    if (-not $in) { return $shown[0] }
    if ($in -match '^[1-9][0-9]*$' -and [int]$in -le $shown.Count) { return $shown[[int]$in - 1] }
    if ($cur -contains $in) { return $in }
    $f = @($cur | Where-Object { $_ -like "*$in*" })
    if ($f.Count -eq 0) { Write-Err "  no id matches `"$in`"." } else { $cur = $f }
  }
}
function Get-Providers {
  if (-not (Test-ModelsReady)) { return }
  if (-not (Require-PresetHelper)) { return }
  Write-Host "Cloud providers - bring your own key (your key, the provider bills you):"
  foreach ($ln in (Invoke-ModelsPy @('presets'))) {
    $p = "$ln".Split('|'); if ($p.Count -ge 6 -and $p[5] -eq 'cloud') { Write-Host ("  {0,-12} {1}" -f $p[0], $p[1]) }
  }
  Write-Host "Local servers - your compute, never billed:"
  foreach ($ln in (Invoke-ModelsPy @('presets'))) {
    $p = "$ln".Split('|'); if ($p.Count -ge 6 -and $p[5] -eq 'local') { Write-Host ("  {0,-12} {1}" -f $p[0], $p[1]) }
  }
  Write-Host "Add one:  byteask models add <alias> --provider <id> [--key K] [--model ID]"
}
# Returns $true on success, $false on any failure (so the settings screen + exit code can react).
function Add-Model([string[]]$rest) {
  if (-not (Test-ModelsReady)) { return $false }
  if ($rest.Count -lt 1) { Write-Err "usage: byteask models add <alias> --provider <id> | --url <endpoint>"; return $false }
  $alias = $rest[0] -replace '^self/',''
  if (-not $alias -or $alias.StartsWith('-')) { Write-Err "usage: byteask models add <alias> --provider <id> | --url <endpoint>"; return $false }
  # Seed the key from BK_KEY so a caller can pass it out-of-argv (a command line is
  # readable by other processes) - the same channel this function already uses to reach
  # the helper. An explicit --key is parsed below and still wins.
  $u=''; $mid=''; $key="$env:BK_KEY"; $wire='auto'; $wireSet=$false; $ctx=''; $tools='auto'; $disp=''; $ca=''; $ins=$false; $hdrs=@(); $ollama=$false; $provider=''; $providerId=''; $kind='self'
  for ($i=1; $i -lt $rest.Count; $i++) {
    switch -Regex ($rest[$i]) {
      '^--provider$'  { $provider = $rest[++$i] }
      '^--url$'       { $u = $rest[++$i] }
      '^--model$'     { $mid = $rest[++$i] }
      '^--key$'       { $key = $rest[++$i] }
      '^--wire$'      { $wire = $rest[++$i]; $wireSet = $true }
      '^--ctx$'       { $ctx = $rest[++$i] }
      '^--tools$'     { $tools = $rest[++$i] }
      '^--name$'      { $disp = $rest[++$i] }
      '^--ca-bundle$' { $ca = $rest[++$i] }
      '^--insecure$'  { $ins = $true }
      '^--header$'    { $hdrs += $rest[++$i] }
      '^--ollama$'    { $ollama = $true }
      default { Write-Err "byteask models add: unknown option $($rest[$i])"; return $false }
    }
  }
  $py = Get-ModelsPython
  $insArg = if ($ins) { '1' } else { '' }
  # --ollama is an alias for --provider ollama on a preset-capable helper; else the
  # historical localhost default (no version gate for existing ollama users).
  if ($ollama -and -not $provider) {
    if (Test-PresetHelper) { $provider = 'ollama' } elseif (-not $u) { $u = 'http://localhost:11434' }
  }
  # Resolve a provider preset -> fills URL + wire + key env + kind. --url / --wire win.
  if ($provider) {
    if (-not (Require-PresetHelper)) { return $false }
    $preset = Get-ProviderPreset $provider
    if (-not $preset) { Write-Err "byteask models add: unknown provider '$provider'. See: byteask models providers"; return $false }
    $providerId = $provider; $kind = $preset.kind
    if (-not $u) { $u = $preset.url }
    if (-not $wireSet) { $wire = $preset.wire }
    if ($preset.needs -and -not $key) {
      if ($preset.env) { $key = [Environment]::GetEnvironmentVariable($preset.env) }
      if (-not $key -and (Test-Interactive)) {
        $key = Read-HiddenLine "  $provider API key (input hidden; Enter to abort)"
      }
      if (-not $key) { Write-Err "byteask models add: $provider needs an API key - pass --key, or set `$$($preset.env)."; return $false }
    }
  }
  if (-not $u) { Write-Err "byteask models add: pass --provider <id> or --url <endpoint>."; return $false }
  if (-not $mid) {
    Write-Err "  probing $u for available models..."
    $ids = @(Invoke-Discover $u $key $ca $insArg)
    if (-not $ids -or $ids.Count -eq 0) { return $false }
    if (Test-Interactive) {
      $mid = Select-FromIds $ids
    } elseif ($kind -eq 'cloud') {
      Write-Err "byteask models add: $provider lists many models - pass --model <id>. For example:"
      foreach ($ex in @($ids | Select-Object -First 3)) { Write-Err "    $ex" }
      return $false
    } else { $mid = $ids[0] }
    if (-not $mid) { Write-Err "  no model selected."; return $false }
    Write-Err "  model: $mid"
  }
  if ($wire -eq 'auto') {
    $prev = $env:BK_KEY; $env:BK_KEY = $key
    try { $w = @(& $py $BYOK_MODELS probe-wire $u $mid '' $ca $insArg 2>$null) }
    finally { if ($null -eq $prev) { Remove-Item Env:BK_KEY -ErrorAction SilentlyContinue } else { $env:BK_KEY = $prev } }
    $wire = if ($w -and $w.Count -ge 1) { "$($w[0])".Trim() } else { 'chat' }
    Write-Err "  wire: $wire"
  }
  $ep = @{ base_url = $u; model_id = $mid; wire = $wire; tools = $tools }
  if ($key)        { $ep.api_key = $key }
  if ($ctx)        { $ep.context_window = [int]$ctx }
  if ($disp)       { $ep.display_name = $disp }
  if ($ca)         { $ep.ca_bundle = $ca }
  if ($ins)        { $ep.insecure = $true }
  if ($providerId) { $ep.provider_id = $providerId }
  if ($hdrs.Count -gt 0) {
    $h = @{}
    foreach ($x in $hdrs) { $p = $x -split '=', 2; if ($p.Count -eq 2) { $h[$p[0].Trim()] = $p[1].Trim() } }
    if ($h.Count -gt 0) { $ep.headers = $h }
  }
  $json = ($ep | ConvertTo-Json -Compress -Depth 5)
  $json | & $py $BYOK_MODELS add $BYOK_CFG $alias | Out-Null
  if ($LASTEXITCODE -ne 0) { Write-Err "  failed to save the endpoint."; return $false }
  if (-not (Invoke-ByokEnter)) { Write-Err "  (warning: couldn't start the local sidecar - run 'byteask' to retry)" }
  Invoke-ModelsMerge
  if ($kind -eq 'cloud') {
    Write-Host "Added self/$alias  ($mid via $provider). Runs on $provider's cloud under YOUR key - they bill you; agent loops consume credits."
  } else {
    Write-Host "Added self/$alias  ($mid, wire=$wire). Your compute - never billed by ByteAsk."
  }
  Write-Host "Pick it in /model, or run:  byteask --model self/$alias"
  if (Test-Interactive) {
    $ans = Read-Host "  Run a quick test now (one request on your key)? [Y/n]"
    if ($ans -match '^[Nn]') { Write-Err "  Skipped. Test later:  byteask models test $alias" }
    else { Test-Model @($alias) | Out-Null }
  } else {
    Write-Host "Test it end-to-end:  byteask models test $alias"
  }
  return $true
}
function Get-Models([string[]]$rest) {
  if (-not (Test-ModelsReady)) { return }
  $py = Get-ModelsPython
  & $py $BYOK_MODELS list $BYOK_CFG
  if (($rest.Count -ge 1) -and ($rest[0] -eq '--check')) {
    Write-Host "  checking reachability..."
    $json = (& $py $BYOK_MODELS list $BYOK_CFG --json 2>$null)
    try { $eps = ("$json" | ConvertFrom-Json) } catch { $eps = $null }
    if ($eps) {
      foreach ($al in $eps.PSObject.Properties.Name) {
        $ux = (& $py $BYOK_MODELS get $BYOK_CFG $al base_url 2>$null)
        $kx = (& $py $BYOK_MODELS get $BYOK_CFG $al api_key 2>$null)
        $prev = $env:BK_KEY; $env:BK_KEY = "$kx"
        try { (& $py $BYOK_MODELS discover "$ux" 2>$null) | Out-Null }
        finally { if ($null -eq $prev) { Remove-Item Env:BK_KEY -ErrorAction SilentlyContinue } else { $env:BK_KEY = $prev } }
        if ($LASTEXITCODE -eq 0) { Write-Host "    self/$($al): reachable" } else { Write-Host "    self/$($al): UNREACHABLE" }
      }
    }
  }
}
function Test-Model([string[]]$rest) {
  if (-not (Test-ModelsReady)) { return }
  if ($rest.Count -lt 1) { Write-Err "usage: byteask models test <alias>"; return }
  $a = $rest[0] -replace '^self/',''
  $py = Get-ModelsPython
  & $py $BYOK_MODELS test $BYOK_CFG $a
}
function Remove-Model([string[]]$rest) {
  if (-not (Test-ModelsReady)) { return }
  if ($rest.Count -lt 1) { Write-Err "usage: byteask models remove <alias>"; return }
  $a = $rest[0] -replace '^self/',''
  $py = Get-ModelsPython
  $res = (& $py $BYOK_MODELS remove $BYOK_CFG $a 2>$null)
  $cur = Get-CfgLine '^model = "(.*)"$'
  if ($cur -eq "self/$a") {
    $def = if ($env:BYTEASK_MODEL) { $env:BYTEASK_MODEL } else { 'gpt-5.4' }
    $cfgp = Join-Path $CODEX_HOME 'config.toml'
    (Get-Content $cfgp) -replace "^model = ""self/$a""$", "model = ""$def""" | Set-Content $cfgp
    Write-Host "  (was your active model; switched to $def)"
  }
  Invoke-ModelsMerge
  if ("$res".Trim() -eq 'removed') { Write-Host "Removed self/$a." } else { Write-Host "No self-hosted model 'self/$a'." }
}
# Returns an exit code (0 ok, non-zero on failure) so the top-level dispatch can
# propagate it instead of always exiting 0 (a scriptable failure signal).
function Invoke-Models([string[]]$rest) {
  $sub = if ($rest.Count -ge 1) { $rest[0] } else { '' }
  $tail = @($rest | Select-Object -Skip 1)
  switch ($sub) {
    'add'       { if (Add-Model $tail) { return 0 } else { return 1 } }
    'providers' { Get-Providers; return 0 }
    'list'      { Get-Models $tail; return 0 }
    'ls'        { Get-Models $tail; return 0 }
    'test'      { Test-Model $tail; return 0 }
    'remove'    { Remove-Model $tail; return 0 }
    'rm'        { Remove-Model $tail; return 0 }
    { $_ -eq '' -or $_ -eq 'help' -or $_ -eq '--help' -or $_ -eq '-h' } { Show-ModelsHelp; return 0 }
    default     { Write-Err "usage: byteask models <add|providers|list|test|remove>"; return 2 }
  }
}

# Looping per-provider manage view. Keys mix (OpenAI + Anthropic + Gemini can all be
# set); un-keyed providers use ByteAsk managed. No-op when non-interactive. Keys are
# typed at a masked prompt, never shown.
# Signed-in email = the "sub" claim of the JWT (base64url middle segment), decoded via
# .NET (no python dep). Empty on any failure.
function Get-CurrentEmail {
  $jwt = Get-CurrentJwt
  if (-not $jwt) { return '' }
  try {
    $seg = $jwt.Split('.')[1].Replace('-','+').Replace('_','/')
    switch ($seg.Length % 4) { 2 { $seg += '==' } 3 { $seg += '=' } }
    $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($seg))
    return ([regex]::Match($json, '"sub"\s*:\s*"([^"]*)"').Groups[1].Value)
  } catch { return '' }
}

# Arrow-navigable single-select. Returns the chosen 1-based index (0 = cancel). Up/Down
# move; Enter/Space select; 1-9 jump-select; q/Esc cancel. Falls back to a numbered prompt
# when input is redirected (pipes / tests).
function Select-Menu([string[]]$Options, [int]$Start = 1) {
  $n = $Options.Count
  if ($env:BYOK_ASSUME_TTY -or [Console]::IsInputRedirected) {
    for ($k = 0; $k -lt $n; $k++) { [Console]::Error.WriteLine("  $($k+1)) $($Options[$k])") }
    $s = Read-Host '> '
    if ($s -match '^[1-9][0-9]*$' -and [int]$s -ge 1 -and [int]$s -le $n) { return [int]$s }
    return 0
  }
  $i = [Math]::Max(0, [Math]::Min($n - 1, $Start - 1))   # initial cursor (1-based $Start, clamped)
  try { $top = [Console]::CursorTop } catch { $top = 0 }
  while ($true) {
    try { [Console]::SetCursorPosition(0, $top) } catch {}
    for ($k = 0; $k -lt $n; $k++) {
      $line = $(if ($k -eq $i) { '> ' } else { '  ' }) + $Options[$k]
      $pad = [Math]::Max(0, [Console]::WindowWidth - 1 - $line.Length)
      if ($k -eq $i) { Write-Host ($line + (' ' * $pad)) -ForegroundColor Cyan }
      else { Write-Host ($line + (' ' * $pad)) }
    }
    $key = [Console]::ReadKey($true)
    switch ($key.Key) {
      'UpArrow'   { $i = ($i - 1 + $n) % $n }
      'DownArrow' { $i = ($i + 1) % $n }
      'Enter'     { return ($i + 1) }
      'Spacebar'  { return ($i + 1) }
      'Escape'    { return 0 }
      default {
        $c = [string]$key.KeyChar
        if ($c -eq 'q' -or $c -eq 'Q') { return 0 }
        if ($c -match '^[1-9]$' -and [int]$c -le $n) { return [int]$c }
      }
    }
  }
}

# ==================== persistent settings screen (PS port) ==================
# Same anchored in-place region as the sh wrapper (docs/menu-redesign-plan.md):
# header, breadcrumb, menu, status line, footer repainted over themselves each
# frame. All console I/O flows through the $script:ScrIO ADAPTER (D6#8) so the
# state machine + geometry run under a MOCK console in automated tests; only
# the final visual pass needs a real Windows console. KEEP THIS FILE PURE ASCII.
$script:ScrIO = @{
  GetSize   = { ,@([Console]::WindowHeight, [Console]::WindowWidth) }
  CursorTop = { [Console]::CursorTop }
  SetPos    = { param($r) [Console]::SetCursorPosition(0, [Math]::Max(0, $r)) }
  WriteRow  = { param($text, $color, $noNL)
                if ($color) { Write-Host $text -ForegroundColor $color -NoNewline:$noNL }
                else        { Write-Host $text -NoNewline:$noNL } }
  ReadKey   = { [Console]::ReadKey($true) }
  KeyAvail  = { [Console]::KeyAvailable }
  CtrlC     = { param($on) try { [Console]::TreatControlCAsInput = $on } catch {} }
  ReadSecret= { $s = Read-Host -AsSecureString ' '
                [Runtime.InteropServices.Marshal]::PtrToStringAuto(
                  [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)) }
  ReadLine  = { Read-Host ' ' }
}
function Set-ScrIOMock([hashtable]$Mock) { $script:ScrIO = $Mock }   # test seam (T27)

function Get-ScrGlyphs {
  # Unicode glyphs only where fonts are dependable (Windows Terminal); legacy
  # conhost raster fonts get ASCII. Emitted via [char] - the FILE stays ASCII.
  if ($env:WT_SESSION -or $env:TERM_PROGRAM) {
    return @{ ok=[string][char]0x2713; x=[string][char]0x2717; dot=[string][char]0x00B7
              bc=[string][char]0x203A; ud=([string][char]0x2191 + [string][char]0x2193) }
  }
  return @{ ok='OK'; x='x'; dot='-'; bc='>'; ud='Up/Down' }
}

function Test-ScrOk {
  if ($env:BYTEASK_PLAIN_MENU) { return $false }
  if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) { return $false }
  if ($env:BYOK_ASSUME_TTY) { return $false }   # test harnesses pipe the numbered path
  try {
    $sz = & $script:ScrIO.GetSize
    if ($sz[0] -lt 16 -or $sz[1] -lt 40) { return $false }
    [void](& $script:ScrIO.CursorTop)
    return $true
  } catch { return $false }   # ISE / hosts without a real console buffer
}

function Get-ScrData {
  $v = Get-ByokKeyVerbs
  $script:ScrD = @{
    Email = Get-CurrentEmail
    VOA = $v[0]; VAN = $v[1]; VGE = $v[2]
    NKeys = Get-ByokKeyCount
    Keyed = @()
    NSelf = 0; SelfRows = @()
    Signed = (Test-SignedIn)
  }
  if (-not $script:ScrD.Email) { $script:ScrD.Email = 'not signed in' }
  if ($v[0] -eq 'change') { $script:ScrD.Keyed += 'openai' }
  if ($v[1] -eq 'change') { $script:ScrD.Keyed += 'anthropic' }
  if ($v[2] -eq 'change') { $script:ScrD.Keyed += 'gemini' }
  if ((Test-Path $BYOK_MODELS) -and (Get-ModelsPython) -and (Test-Path $BYOK_CFG)) {
    $rows = @(Invoke-ModelsPy @('list', $BYOK_CFG))
    foreach ($r in $rows) { if ("$r" -match 'self/') { $script:ScrD.SelfRows += "$r"; $script:ScrD.NSelf++ } }
  }
  $g = $script:ScrG
  $pk = @{ $true='yours'; $false='managed' }
  $script:ScrD.KLine = "Keys: OpenAI " + $pk[($v[0] -eq 'change')] + " $($g.dot) Anthropic " +
    $pk[($v[1] -eq 'change')] + " $($g.dot) Gemini " + $pk[($v[2] -eq 'change')]
}
function Get-ScrProvName([string]$p) {
  switch ($p) { 'openai' {'OpenAI'} 'anthropic' {'Anthropic'} 'gemini' {'Gemini'} default {$p} }
}

function Enter-ScrKeys {
  $script:ScrState = 'keys'; Get-ScrData
  $o = @()
  $o += ,@('openai',    ("OpenAI      " + $script:ScrD.VOA + " your key"))
  $o += ,@('anthropic', ("Anthropic   " + $script:ScrD.VAN + " your key"))
  $o += ,@('gemini',    ("Gemini      " + $script:ScrD.VGE + " your key"))
  if ($script:ScrD.NKeys -gt 0) { $o += ,@('remove', 'Remove a key') }
  $o += ,@('selfhost', ("Your own hosted model $($script:ScrG.dot) vLLM, TGI, Ollama, LM Studio, DGX, or any OpenAI-compatible server"))
  $o += ,@('managed',  ("Use ByteAsk managed $($script:ScrG.dot) 20 models incl. GPT, Gemini & open models, 20% off API pricing"))
  $o += ,@('done',     'Done')
  $script:ScrOpts = $o
  $script:ScrCur = if ($script:ScrD.NKeys -eq 0) { $o.Count - 1 } else { $o.Count }
}
function Enter-ScrAccount {
  $script:ScrState = 'account'; Get-ScrData
  $script:ScrOpts = @(,@('keys','Manage keys & models'),
                      ,@('email','Sign in with a different email'),
                      ,@('done','Done'))
  $script:ScrCur = 1
}
function Enter-ScrRemove {
  $script:ScrState = 'remove'
  $o = @()
  foreach ($p in $script:ScrD.Keyed) { $o += ,@($p, ((Get-ScrProvName $p) + "      (your key)")) }
  $o += ,@('cancel','Cancel')
  $script:ScrOpts = $o; $script:ScrCur = 1
}
function Enter-ScrSelfhost {
  $script:ScrState = 'selfhost'; Get-ScrData
  $script:ScrOpts = @(,@('add','Add a hosted model now'), ,@('back','Back'))
  $script:ScrCur = 1
}
function Enter-ScrProvider {   # navigable provider picker: presets first, then custom URL
  $script:ScrState = 'provider'
  $o = @()
  if (Test-PresetHelper) {
    $all = @(Invoke-ModelsPy @('presets'))
    foreach ($want in @('cloud','local')) {
      foreach ($ln in $all) {
        $p = "$ln".Split('|'); if ($p.Count -lt 6 -or $p[5] -ne $want) { continue }
        $bill = if ($want -eq 'cloud') { 'they bill you' } else { 'never billed' }
        $lab = ("{0,-12}{1}  $($script:ScrG.dot) {2}" -f $p[0], $p[1], $bill)
        $o += ,@($p[0], $lab)
      }
    }
  }
  $o += ,@('custom', "Custom server $($script:ScrG.dot) enter your own URL")
  $o += ,@('back', 'Back')
  $script:ScrOpts = $o
  # default cursor on "custom" (2nd-to-last) - bare Enter keeps the old
  # muscle-memory of "Enter = use your own URL"; arrows still reach every preset.
  $script:ScrCur = $o.Count - 1
}

function Get-ScrRows {   # the frame as (text,color,indent) rows - pure data, unit-testable
  $g = $script:ScrG; $d = $script:ScrD
  $rows = @()
  $rows += ,@(("ByteAsk $($g.dot) Settings"), 'Cyan')
  if ($d.Signed) { $rows += ,@(($d.Email + " $($g.dot) signed in"), 'DarkGray') }
  else { $rows += ,@(("not signed in $($g.dot) own keys & hosted models only"), 'DarkGray') }
  $rows += ,@(($d.KLine), 'DarkGray')
  $rows += ,@('', $null)
  switch ($script:ScrState) {
    'keys'     { $rows += ,@('API keys', 'White')
                 $rows += ,@(("OpenAI, Anthropic, or Gemini $($g.dot) bring your own key, billed directly to you"), 'DarkGray') }
    'account'  { $rows += ,@('Account', 'White') }
    'remove'   { $rows += ,@(("API keys $($g.bc) Remove"), 'White') }
    'selfhost' { $rows += ,@(("API keys $($g.bc) Your own hosted model"), 'White')
                 if ($d.SelfRows.Count -gt 0) { foreach ($r in $d.SelfRows) { $rows += ,@($r.TrimEnd(), $null) } }
                 else { $rows += ,@('No hosted models yet.', $null) } }
    'provider' { $rows += ,@(("API keys $($g.bc) Your own hosted model $($g.bc) Choose a provider"), 'White') }
    'prompt'   { $rows += ,@(("API keys $($g.bc) " + (Get-ScrProvName $script:ScrP)), 'White')
                 $rows += ,@(("Paste your " + (Get-ScrProvName $script:ScrP) + " API key (input stays hidden):"), $null) }
    'confirm'  { $rows += ,@(("API keys $($g.bc) Use ByteAsk managed"), 'White')
                 $rows += ,@(("This removes your " + $script:ScrD.NKeys + " saved key(s) (" + ($script:ScrD.Keyed -join ', ') + ")."), $null) }
  }
  if ($script:ScrState -in @('keys','account','remove','selfhost','provider')) {
    for ($i = 0; $i -lt $script:ScrOpts.Count; $i++) {
      $lab = $script:ScrOpts[$i][1]
      if (($i + 1) -eq $script:ScrCur) { $rows += ,@(("> " + $lab), 'Cyan', $true) }
      else { $rows += ,@(("  " + $lab), $null, $true) }
    }
  }
  if ($script:ScrState -eq 'remove') { $rows += ,@(("Removing only deletes the saved key $($g.dot) add it again anytime"), 'DarkGray') }
  if ($script:ScrState -eq 'selfhost') { $rows += ,@(("A cloud provider with your key (they bill you), or your own server (never billed)"), 'DarkGray') }
  switch ($script:ScrMsgK) {
    'ok'   { $rows += ,@(("$($g.ok) " + $script:ScrMsg), 'Cyan') }
    'err'  { $rows += ,@(("$($g.x) " + $script:ScrMsg), 'Red') }
    'info' { $rows += ,@(($script:ScrMsg), 'DarkGray') }
    default{ $rows += ,@('', $null) }
  }
  switch ($script:ScrState) {
    'prompt'  { $rows += ,@(("Enter submit $($g.dot) empty Enter cancels"), 'DarkGray') }
    'confirm' { $rows += ,@(("y confirm $($g.dot) anything else cancels"), 'DarkGray') }
    'account' { $rows += ,@(("$($g.ud) move $($g.dot) Enter select $($g.dot) Esc close"), 'DarkGray') }
    default   { $rows += ,@(("$($g.ud) move $($g.dot) Enter select $($g.dot) Esc back"), 'DarkGray') }
  }
  return ,$rows
}

function Show-ScrFrame {
  $sz = & $script:ScrIO.GetSize
  $width = [Math]::Max(20, $sz[1] - 2)
  $rows = Get-ScrRows
  if ($script:ScrDrawn -gt 0) {
    # relative top recompute EVERY frame (the R2 fix): the last row was written
    # -NoNewline, so the region's top is CursorTop - (drawn - 1)
    $top = (& $script:ScrIO.CursorTop) - ($script:ScrDrawn - 1)
    & $script:ScrIO.SetPos $top
  }
  $n = $rows.Count
  for ($i = 0; $i -lt $n; $i++) {
    $r = $rows[$i]
    $txt = "$($r[0])"
    $indent = ($r.Count -ge 3 -and $r[2])
    if (-not $indent) { $txt = "  " + $txt }
    if ($txt.Length -gt $width) { $txt = $txt.Substring(0, $width) }
    $txt = $txt.PadRight($width)
    & $script:ScrIO.WriteRow $txt $r[1] ($i -eq ($n - 1))   # last row -NoNewline (D6#9)
  }
  # a shorter frame leaves orphan rows below: blank them, then reposition
  if ($n -lt $script:ScrDrawn) {
    $orphans = $script:ScrDrawn - $n
    & $script:ScrIO.WriteRow '' $null $false
    for ($i = 0; $i -lt $orphans; $i++) { & $script:ScrIO.WriteRow (' ' * $width) $null ($i -eq ($orphans - 1)) }
    $top = (& $script:ScrIO.CursorTop) - ($n + $orphans - 1)
    & $script:ScrIO.SetPos ($top + $n - 1)
  }
  $script:ScrDrawn = $n
}

function Read-ScrKey {   # -> up|down|enter|esc|paste|ctrlc|eof|none
  try { $k = & $script:ScrIO.ReadKey } catch { return 'eof' }
  if ($null -eq $k) { return 'eof' }
  if (($k.Modifiers -band [ConsoleModifiers]::Control) -and $k.Key -eq 'C') { return 'ctrlc' }
  switch ($k.Key) {
    'UpArrow'   { return 'up' }
    'DownArrow' { return 'down' }
    'Escape'    { return 'esc' }
    { $_ -in 'Enter','Spacebar' } {
      # burst-Enter guard: an Enter with input right behind it is a paste tail
      Start-Sleep -Milliseconds 25
      if (& $script:ScrIO.KeyAvail) { Clear-ScrBurst; return 'paste' }
      return 'enter'
    }
    default {
      # printable / other keys: DEAD in the raw menu (D5). A burst behind one
      # printable char means a paste - swallow it whole with an explanation.
      Start-Sleep -Milliseconds 25
      if (& $script:ScrIO.KeyAvail) { Clear-ScrBurst; return 'paste' }
      return 'none'
    }
  }
}
function Clear-ScrBurst {
  $idle = 0
  while ($idle -lt 3) {
    while (& $script:ScrIO.KeyAvail) { [void](& $script:ScrIO.ReadKey); $idle = 0 }
    Start-Sleep -Milliseconds 40
    $idle++
  }
}

function Invoke-ScrAddKey([string]$prov) {
  $pn = Get-ScrProvName $prov
  if (-not (Test-Path $BYOK_SIDECAR)) {
    $script:ScrMsgK='err'; $script:ScrMsg="BYOK needs the sidecar $($script:ScrG.dot) run 'byteask --update' first"; return
  }
  if (-not (Get-PyExe)) {
    $script:ScrMsgK='err'; $script:ScrMsg='BYOK needs python (not found on PATH)'; return
  }
  $script:ScrState = 'prompt'; $script:ScrP = $prov
  $script:ScrMsg=''; $script:ScrMsgK='none'
  Show-ScrFrame
  & $script:ScrIO.WriteRow '' $null $false          # move below the region for the OS prompt
  $key = ''
  try { $key = & $script:ScrIO.ReadSecret } catch { $key = '' }
  $script:ScrDrawn += 1                             # the prompt row joins the region
  if (-not $key) { $script:ScrMsgK='info'; $script:ScrMsg='Cancelled'; Enter-ScrKeys; return }
  Enter-ScrKeys
  $script:ScrMsgK='info'; $script:ScrMsg="Checking your key with $pn..."
  Show-ScrFrame                                     # visible during (<=15s) validation
  if (Test-ProviderKey $prov $key 6>$null 2>$null) {
    Invoke-ByokMerge @("keys.$prov=$key"); $key = ''
    $entered = $false
    try { $entered = (Invoke-ByokEnter 6>$null 2>$null) } catch { $entered = $false }
    if ($entered) { $script:ScrMsgK='ok'; $script:ScrMsg="Saved your $pn key" }
    else { $script:ScrMsgK='err'; $script:ScrMsg="Couldn't start the local helper $($script:ScrG.dot) run 'byteask --update'" }
  } else {
    $key = ''
    $script:ScrMsgK='err'; $script:ScrMsg="$pn rejected that key $($script:ScrG.dot) check and paste again"
  }
  Enter-ScrKeys
}
function Invoke-ScrRemove([string]$prov) {
  $pn = Get-ScrProvName $prov
  try { Invoke-ByokRemove @($prov) 6>$null 2>$null } catch {}
  Enter-ScrKeys
  if ($script:ScrD.NKeys -eq 0) {
    $script:ScrMsgK='ok'; $script:ScrMsg="Removed your $pn key $($script:ScrG.dot) no keys left, everything uses ByteAsk managed"
  } else { $script:ScrMsgK='ok'; $script:ScrMsg="Removed your $pn key" }
}
function Invoke-ScrManaged {
  if ($script:ScrD.NKeys -eq 0) {
    try { Invoke-ByokOff 6>$null 2>$null } catch {}
    $script:ScrExit = $true; return
  }
  $script:ScrState = 'confirm'; $script:ScrMsg=''; $script:ScrMsgK='none'
  Clear-ScrBurst                                    # drain queued Enters (S3.5#4)
  Show-ScrFrame
  $ck = $null
  try { $ck = & $script:ScrIO.ReadKey } catch {}
  if ($ck -and "$($ck.KeyChar)".ToLower() -eq 'y') {
    $n = $script:ScrD.NKeys
    try { Invoke-ByokOff 6>$null 2>$null } catch {}
    Enter-ScrKeys
    $script:ScrMsgK='ok'; $script:ScrMsg="Switched to ByteAsk managed $($script:ScrG.dot) $n saved key(s) removed"
    $script:ScrExit = $true
  } else { $script:ScrMsgK='info'; $script:ScrMsg='Cancelled'; Enter-ScrKeys }
}
function Read-ScrName {   # prompts Name; returns trimmed alias ('' = cancel)
  Write-Host -NoNewline '  Name it (e.g. my-dgx): '
  $al = ''; try { $al = & $script:ScrIO.ReadLine } catch {}
  return "$al".Trim()
}
function Read-ScrKeyOpt {   # prompts optional API key (hidden input); returns trimmed key ('' = skip)
  Write-Host -NoNewline '  API key, if needed (Enter to skip; input hidden):'
  $k = ''; try { $k = & $script:ScrIO.ReadSecret } catch { $k = '' }
  return "$k".Trim()
}
function Complete-ScrSelfhostAdd([string]$al, [string[]]$addArgs) {
  $okAdd = $false
  try { $okAdd = [bool](Add-Model $addArgs) } catch { $okAdd = $false }
  if ($okAdd -and (Test-HasSelfEndpoints)) {
    $script:ScrMsgK='ok'; $script:ScrMsg="Added self/$al $($script:ScrG.dot) pick it in /model"
  } else { $script:ScrMsgK='err'; $script:ScrMsg="Couldn't add it $($script:ScrG.dot) details above" }
  & $script:ScrIO.CtrlC $true
  $script:ScrDrawn = 0                               # fresh region below the wizard output
  Enter-ScrSelfhost
}
function Invoke-ScrSelfAddPreset([string]$pid) {   # name + optional key, provider already chosen
  & $script:ScrIO.WriteRow '' $null $false
  & $script:ScrIO.CtrlC $false
  $al = Read-ScrName
  if (-not $al) { & $script:ScrIO.CtrlC $true; $script:ScrDrawn = 0
                  $script:ScrMsgK='info'; $script:ScrMsg='Cancelled'; Enter-ScrSelfhost; return }
  $k = Read-ScrKeyOpt
  $addArgs = @($al, '--provider', $pid)
  if ($k) { $addArgs += @('--key', $k) }
  $k = ''
  Complete-ScrSelfhostAdd $al $addArgs
}
function Invoke-ScrSelfAddCustom {   # name + URL + optional key, then hand-off to the wizard (T16)
  & $script:ScrIO.WriteRow '' $null $false
  & $script:ScrIO.CtrlC $false
  $al = Read-ScrName
  if (-not $al) { & $script:ScrIO.CtrlC $true; $script:ScrDrawn = 0
                  $script:ScrMsgK='info'; $script:ScrMsg='Cancelled'; Enter-ScrSelfhost; return }
  Write-Host -NoNewline '  Server URL (e.g. http://dgx-host:8000): '
  $u = ''; try { $u = & $script:ScrIO.ReadLine } catch {}
  $u = "$u".Trim()
  if (-not $u) { & $script:ScrIO.CtrlC $true; $script:ScrDrawn = 0
                 $script:ScrMsgK='info'; $script:ScrMsg='Cancelled'; Enter-ScrSelfhost; return }
  $k = Read-ScrKeyOpt
  $addArgs = @($al, '--url', $u)
  if ($k) { $addArgs += @('--key', $k) }
  $k = ''
  Complete-ScrSelfhostAdd $al $addArgs
}

function Invoke-ScrSelect {   # $true = keep looping, $false = leave the flow
  $tag = $script:ScrOpts[$script:ScrCur - 1][0]
  switch ("$($script:ScrState)/$tag") {
    'keys/openai'    { Invoke-ScrAddKey 'openai' }
    'keys/anthropic' { Invoke-ScrAddKey 'anthropic' }
    'keys/gemini'    { Invoke-ScrAddKey 'gemini' }
    'keys/remove'    { Enter-ScrRemove }
    'keys/selfhost'  { Enter-ScrSelfhost }
    'keys/managed'   { Invoke-ScrManaged }
    'keys/done'      { return $false }
    'account/keys'   { Enter-ScrKeys }
    'account/email'  { $script:ScrDrawn = 0; & $script:ScrIO.CtrlC $false
                       Write-Host ''
                       Invoke-Login '' ''            # NB: exits the wrapper on failure (pre-existing ps1 behavior)
                       & $script:ScrIO.CtrlC $true
                       Enter-ScrKeys
                       $script:ScrMsgK='ok'; $script:ScrMsg="Signed in as $($script:ScrD.Email)" }
    'account/done'   { return $false }
    'remove/cancel'  { Enter-ScrKeys; $script:ScrMsgK='none'; $script:ScrMsg='' }
    'selfhost/add'   { Enter-ScrProvider }
    'selfhost/back'  { Enter-ScrKeys }
    'provider/custom' { Invoke-ScrSelfAddCustom }
    'provider/back'   { Enter-ScrSelfhost }
    default          { if ($script:ScrState -eq 'remove') { Invoke-ScrRemove $tag }
                       elseif ($script:ScrState -eq 'provider') { Invoke-ScrSelfAddPreset $tag } }
  }
  if ($script:ScrExit) { return $false }
  return $true
}

function Invoke-ScrSettings([string]$entry) {   # $true = the screen ran
  if (-not (Test-ScrOk)) { return $false }
  $script:ScrG = Get-ScrGlyphs
  $script:ScrDrawn = 0; $script:ScrExit = $false
  $script:ScrMsg = ''; $script:ScrMsgK = 'none'
  $script:ScrTop = $entry
  try {
    & $script:ScrIO.CtrlC $true                     # Ctrl-C arrives as a KEY (S3.5)
    if ($entry -eq 'account') { Enter-ScrAccount } else { Enter-ScrKeys }
    while ($true) {
      Show-ScrFrame
      $k = Read-ScrKey
      switch ($k) {
        'up'    { $script:ScrCur = if ($script:ScrCur -gt 1) { $script:ScrCur - 1 } else { $script:ScrOpts.Count } }
        'down'  { $script:ScrCur = if ($script:ScrCur -lt $script:ScrOpts.Count) { $script:ScrCur + 1 } else { 1 } }
        'enter' { if (-not (Invoke-ScrSelect)) {
                    # an action-driven exit (managed wipe) set a status -> paint it
                    # once so the confirmation lands in the final frame, not lost.
                    if ($script:ScrExit) { Show-ScrFrame }
                    break } }
        'paste' { $script:ScrMsgK='info'
                  $script:ScrMsg="Paste ignored here $($script:ScrG.dot) pick with arrows; paste at the key prompt" }
        'esc'   { if ($script:ScrState -eq 'provider') { Enter-ScrSelfhost }
                  elseif ($script:ScrState -in @('remove','selfhost')) { Enter-ScrKeys }
                  elseif ($script:ScrState -eq 'keys' -and $script:ScrTop -eq 'account') { Enter-ScrAccount }
                  else { break } }
        { $_ -in 'ctrlc','eof' } { break }
        default { }
      }
    }
  } finally {
    & $script:ScrIO.CtrlC $false
    try { Write-Host '' } catch {}
  }
  # one summary line stays in scrollback below the final frame, truncated to the
  # console width like every region row so it can't wrap on a narrow terminal.
  $names = @(); foreach ($p in $script:ScrD.Keyed) { $names += (Get-ScrProvName $p) }
  $sum = if ($names.Count -gt 0) { $names -join ', ' } else { 'all managed' }
  $sumline = "  ByteAsk settings $($script:ScrG.dot) keys: $sum $($script:ScrG.dot) hosted models: $($script:ScrD.NSelf).  /login reopens."
  try { $sw = (& $script:ScrIO.GetSize)[1]; if ($sumline.Length -gt $sw) { $sumline = $sumline.Substring(0, $sw) } } catch {}
  Write-Host $sumline
  return $true
}

# ---- sequential fallback (piped stdin, PLAIN_MENU, ISE, redirected hosts) ----
function Show-SourceMenuSeq {
  while ($true) {
    Write-Host ""
    Write-Host "Set up your keys (key as many providers as you like; un-keyed ones use ByteAsk managed):"
    Write-Host ("  Current:  " + (Get-ByokStatusLine))
    Write-Host "Keyed providers bill to your account; everything else uses ByteAsk managed (counts toward your usage)."
    Write-Host "Use Up/Down + Enter, or type a number:"
    # First login (no keys): default the cursor to "Use ByteAsk managed" so a bare Enter
    # just uses managed; once keys are set, default to "Done" so a stray Enter never wipes them.
    $start = if ((Get-ByokKeyCount) -eq 0) { 6 } else { 7 }
    $v = Get-ByokKeyVerbs   # "change" if that provider's key is set, else "add"
    $c = Select-Menu @(
      "OpenAI     - $($v[0]) key",
      "Anthropic  - $($v[1]) key",
      "Gemini     - $($v[2]) key",
      "Remove one of your keys",
      "Use your own hosted model - vLLM, TGI, Ollama, LM Studio, DGX, or any OpenAI-compatible server",
      "Use ByteAsk managed - 20 models incl. GPT, Gemini & open models, 20% off API pricing",
      "Done") $start
    switch ($c) {
      1 { [void](Add-ByokKey 'openai') }
      2 { [void](Add-ByokKey 'anthropic') }
      3 { [void](Add-ByokKey 'gemini') }
      4 { $rp = Read-Host "Remove which key? [openai/anthropic/gemini]"
          if (@('openai','anthropic','gemini') -contains $rp) { Invoke-ByokRemove @($rp) } else { Write-Host "  (unknown provider)" } }
      5 { Show-SelfHostedHowto }   # self-hosted models: show the how-to, keep the menu open
      6 { $nk = Get-ByokKeyCount
          if ($nk -ne 0) {   # D3 parity: confirm before wiping saved keys
            $cf = Read-Host "This removes your $nk saved key(s). Type y to confirm"
            if ("$cf".ToLower() -eq 'y') { Invoke-ByokOff; return } else { Write-Host "  (cancelled)" }
          } else { Invoke-ByokOff; return } }
      default { return }   # 7 (Done) or 0 (cancel); ChatGPT-subscription dropped from menu 2026-07-12
    }
  }
}
function Show-SourceMenu {
  if (-not (Test-MenuTty)) { return }
  if (Invoke-ScrSettings 'keys') { return }
  Show-SourceMenuSeq
}

# `/login` + `byteask login`: EMAIL compulsory (every user), THEN the source menu. A
# signed-in user gets a two-choice (change source / different email).
# Finish registering a hosted model whose provider was already chosen in the engine's
# in-TUI settings screen ($ProviderId = preset id, or 'custom' for a hand-entered URL).
# Mirrors the sh wrapper's do_add_model: only the name/URL/key prompts live here, then
# Add-Model does discovery, the wire probe and the catalog merge.
# Register a hosted model whose fields were ALL collected in the engine's in-TUI form
# ($Spec = "<provider> <alias> <url-or-dash>"). Nothing is prompted here: the point of
# the in-TUI form is that the shell never draws an input field.
#
# The API key deliberately does NOT travel in the marker (that file is world-readable,
# and this project already refuses to pass keys through argv for the same reason). The
# engine stages it in an owner-only sibling file; read it once, delete it, pass by env.
function Invoke-RegisterModel([string]$Spec) {
  $parts = ("$Spec".Trim() -split '\s+')
  $secPath = Join-Path $CODEX_HOME '.byteask-auth-secret'
  $k = ''
  if (Test-Path $secPath) {
    try { $k = (Get-Content -Raw $secPath).Trim() } catch { $k = '' }
    Remove-Item -Force $secPath -ErrorAction SilentlyContinue
  }
  if ($parts.Count -lt 2 -or -not $parts[0] -or -not $parts[1]) {
    Write-Err 'byteask: cancelled.'; $k = ''; return
  }
  $p = $parts[0]; $al = $parts[1]
  $u = if ($parts.Count -ge 3) { $parts[2] } else { '' }
  if ($p -eq 'custom') {
    if (-not $u -or $u -eq '-') { Write-Err 'byteask: cancelled (no server URL).'; $k = ''; return }
    $addArgs = @($al, '--url', $u)
  } else {
    $addArgs = @($al, '--provider', $p)
  }
  # BK_KEY, never argv: a command line is readable by other processes.
  $prev = $env:BK_KEY
  try {
    if ($k) { $env:BK_KEY = $k }
    $okAdd = Add-Model $addArgs
  } finally {
    # Restoring a $null with an assignment would leave an empty var behind, not remove
    # it - the same reason every other BK_KEY site here branches on $null.
    if ($null -eq $prev) { Remove-Item Env:BK_KEY -ErrorAction SilentlyContinue } else { $env:BK_KEY = $prev }
    $k = ''
  }
  if (-not $okAdd) { Write-Err "byteask: couldn't add that model - details above." }
}

function Invoke-AddModel([string]$ProviderId) {
  Write-Host -NoNewline 'Name it (e.g. my-dgx): '
  $al = ''; try { $al = [Console]::ReadLine() } catch {}
  $al = "$al".Trim()
  if (-not $al) { Write-Err 'byteask: cancelled.'; return }
  if ($ProviderId -eq 'custom') {
    Write-Host -NoNewline 'Server URL (e.g. http://dgx-host:8000): '
    $u = ''; try { $u = [Console]::ReadLine() } catch {}
    $u = "$u".Trim()
    if (-not $u) { Write-Err 'byteask: cancelled.'; return }
    $addArgs = @($al, '--url', $u)
  } else {
    $addArgs = @($al, '--provider', $ProviderId)
  }
  Write-Host -NoNewline 'API key, if your server needs one (Enter to skip; input hidden): '
  $k = ''
  try {
    $sec = Read-Host -AsSecureString
    $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec)
    try { $k = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
  } catch { $k = '' }
  $k = "$k".Trim()
  if ($k) { $addArgs += @('--key', $k) }
  $k = ''
  $okAdd = $false
  try { $okAdd = [bool](Add-Model $addArgs) } catch { $okAdd = $false }
  if (-not $okAdd) { Write-Err "byteask: couldn't add that model - details above." }
}

function Invoke-Account {
  if (Test-SignedIn) {
    if (-not (Test-MenuTty)) { return }   # signed in + non-interactive: nothing to do
    if (Invoke-ScrSettings 'account') { return }
    $em = Get-CurrentEmail
    if ($em) { Write-Host "You're signed in as $em. Use Up/Down + Enter, or type a number:" }
    else { Write-Host "You're signed in to ByteAsk. Use Up/Down + Enter, or type a number:" }
    $c = Select-Menu @("Change your key / plan", "Sign in with a different email")
    if ($c -eq 2) { Invoke-Login '' '' }
    Show-SourceMenuSeq
  } elseif (-not (Test-NeedsSignin)) {
    # A self-served config (self/* or a cloud model with the user's own key) needs no
    # account. Open the keys/models screen instead of forcing email onboarding; picking
    # "Use ByteAsk managed" there routes to sign-in. Never fires for a managed config.
    if (-not (Test-MenuTty)) { return }
    if (Invoke-ScrSettings 'keys') { return }
    Show-SourceMenuSeq
  } else {
    # Say WHY we're asking. Test-SignedIn is the only expiry-aware check; Get-CurrentEmail
    # reads the JWT's `sub` and ignores `exp`, so an expired user still sees their address
    # everywhere and reads a silent jump to a fresh magic link as "I'm already signed in -
    # why am I logging in again?". A stored-but-rejected JWT means they WERE signed in.
    if (Get-CurrentJwt) {
      $exEm = Get-CurrentEmail
      if ($exEm) { Write-Host "Your ByteAsk session for $exEm has expired - signing in again." }
      else { Write-Host "Your ByteAsk session has expired - signing in again." }
    }
    Invoke-Login '' ''   # email first (compulsory); writes managed config + JWT
    Show-SourceMenu      # then choose source (no-op if non-interactive)
  }
}

# $EmailIsHint: the address came from the engine's in-TUI sign-in form rather than an
# explicit --email. It is still re-promptable, so a refused address (the .ac.in policy)
# asks for a different one instead of exiting the way an explicit --email must.
function Invoke-Login([string]$Email, [string]$Ref, [bool]$EmailIsHint = $false) {
  $gateway = (Resolve-Gateway).TrimEnd('/')
  $model = if ($env:BYTEASK_MODEL) { $env:BYTEASK_MODEL } else { 'gpt-5.4' }
  if (-not $Ref) { $Ref = $env:BYTEASK_REF }
  $refFile = Join-Path $CODEX_HOME 'referral'
  if (-not $Ref -and (Test-Path $refFile)) { $Ref = (Get-Content -Raw $refFile).Trim() }
  if ($Ref -and (($Ref -notmatch '^[A-Za-z0-9_-]+$') -or ($Ref.Length -gt 64))) { $Ref = '' }
  $emailWasArg = ([bool]$Email) -and (-not $EmailIsHint)
  $poll = $null; $link = $null
  # Sign-in loop: ask for the email, VALIDATE it (one gateway probe) BEFORE anything else,
  # and on an unsupported / rejected address show the reason and re-ask instead of exiting.
  # Only ask for a referral code once the email is accepted.
  while ($true) {
    if (-not $Email) { $Email = Read-Host 'Email' }
    $probe = Get-EmailProbe $Email
    if ($probe.Block) {
      Write-Err $probe.Block
      if ($emailWasArg -or -not [Environment]::UserInteractive) { exit 1 }
      Write-Host 'Please enter a different email.'; $Email = ''; continue
    }
    # First signup with no referral code yet - offer to enter one (interactive only;
    # skipped when a -Ref / BYTEASK_REF / install-time code already set $Ref, OR when this
    # email has signed in before - a returning user is never asked). $10 EACH once, one-shot.
    if ((-not $Ref) -and [Environment]::UserInteractive -and (Test-EmailNew $Email $probe.Exists)) {
      $rin = Read-Host 'Have a referral code? You will both get $10 of usage (press Enter to skip)'
      $Ref = ($rin -replace '\s','')
      if ($Ref -and (($Ref -notmatch '^[A-Za-z0-9_-]+$') -or ($Ref.Length -gt 64))) { $Ref = '' }
    }
    Write-Host "Signing in to ByteAsk as $Email ..."
    $bodyObj = if ($Ref) { @{ email = $Email; ref = $Ref } } else { @{ email = $Email } }
    $body = $bodyObj | ConvertTo-Json -Compress
    $err = $null
    try {
      $start = Invoke-RestMethod -Uri "$gateway/auth/start" -Method Post -Body $body -ContentType 'application/json' -ErrorAction Stop
      $poll = $start.poll_token
      $link = $start.dev_magic_link
      if (-not $poll) { $err = if ($start.error) { [string]$start.error } else { 'sign-in failed' } }
    } catch {
      $err = "sign-in failed: $($_.Exception.Message)"
    }
    if ($poll) { break }
    # Unsupported email is caught up front by the probe (re-prompt above); anything here
    # is a network/other error -> show it and exit (don't loop on a transient failure).
    Write-Err $err
    exit 1
  }
  Write-Host "  -> Check $Email for a sign-in link and click it."
  if ($link) {
    Write-Host "  -> (dev) link: $link"
    try { Invoke-RestMethod -Uri $link -TimeoutSec 5 -ErrorAction Stop | Out-Null; Write-Host "  -> sign-in confirmed" } catch { }
  }
  Write-Host "  -> Waiting for confirmation (up to 10 minutes) ..."
  $token = ''
  for ($i = 0; $i -lt 600; $i++) {
    try {
      $r = Invoke-RestMethod -Uri "$gateway/auth/poll" -Method Post -Body (@{ poll_token = $poll } | ConvertTo-Json -Compress) -ContentType 'application/json' -ErrorAction Stop
      if ($r.status -eq 'approved') { $token = $r.access_token; break }
    } catch { }
    # Email delivery can lag and the link often lands in spam - nudge partway through so
    # the wait doesn't look frozen (the poll token stays valid the whole window).
    if ($i -eq 60) { Write-Host "  -> Still waiting - the email can take a minute; check your spam folder." }
    Start-Sleep -Seconds 1
  }
  if (-not $token) {
    Write-Err "sign-in timed out (no confirmation after 10 minutes)."
    Write-Err "  - If the email was slow, run 'byteask login' to send a fresh link."
    Write-Err "  - Or skip email: 'byteask login --with-api-key' to use your own API key."
    exit 1
  }
  New-Item -ItemType Directory -Force -Path $CODEX_HOME | Out-Null
  Set-Content -Path (Join-Path $CODEX_HOME 'gateway') -Value $gateway -NoNewline
  # Model catalog: adds Claude (opus/sonnet) to /model with correct metadata. It
  # REPLACES the engine's bundled catalog, so only reference it after validating the
  # download. Fail-safe: skip on any failure (Claude still routes via the gateway).
  $catalogLine = ""
  $catalogPath = (Join-Path $CODEX_HOME 'models-catalog.json')
  $catalogTmp = "$catalogPath.tmp"
  try {
    Invoke-WebRequest -Uri "$gateway/models-catalog.json" -OutFile $catalogTmp -TimeoutSec 20 -UseBasicParsing -ErrorAction Stop
    if ((Test-Path $catalogTmp) -and (Select-String -Path $catalogTmp -Pattern '"models"' -Quiet)) {
      Move-Item -Force $catalogTmp $catalogPath
      $catalogLine = 'model_catalog_json = "' + ($catalogPath -replace '\\','/') + '"'
    } else { Remove-Item -Force -ErrorAction SilentlyContinue $catalogTmp }
  } catch { Remove-Item -Force -ErrorAction SilentlyContinue $catalogTmp }
  $cfg = @"
model = "$model"
model_provider = "byteask"
web_search = "live"
$catalogLine

[model_providers.byteask]
name = "ByteAsk"
base_url = "$gateway/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "$token"

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
"@
  # A sign-in is only real once the credential is on disk. Never announce success
  # on an unverified write: that is precisely how a real account stayed 401'd for
  # six weeks while every /login reported "Signed in as ..." (2026-09-07).
  if (-not (Save-ConfigToml $cfg $token)) {
    Write-Err "Sign-in did NOT complete - your credential could not be saved."
    exit 1
  }
  Remove-Item -Force -ErrorAction SilentlyContinue $refFile     # one-shot referral
  Add-KnownEmail $Email                                         # so a future re-login skips the referral prompt
  Write-Host "Signed in as $Email. You're ready: byteask `"...`""
}

# Hourly, fail-open update check; on a console it offers y/N and installs in place.
function Invoke-UpdateCheck {
  if ($env:BYTEASK_NO_UPDATE_CHECK) { return }
  $latest = ''; $last = 0
  if (Test-Path $UPDATE_STATE) {
    foreach ($line in Get-Content $UPDATE_STATE) {
      $kv = $line -split '=', 2
      if ($kv.Count -eq 2) {
        if ($kv[0] -eq 'last_check') { [void][int]::TryParse($kv[1], [ref]$last) }
        elseif ($kv[0] -eq 'latest') { $latest = $kv[1] }
      }
    }
  }
  $now = [int][double]::Parse((Get-Date -UFormat %s))
  if ((-not $latest) -or (($now - $last) -ge 3600)) {
    try {
      $gw = (Resolve-Gateway).TrimEnd('/')
      $fetched = ("$(Invoke-RestMethod -Uri "$gw/version" -TimeoutSec 2 -ErrorAction Stop)").Trim()
      if ($fetched -match '^[0-9]+(\.[0-9]+)+$') {
        $latest = $fetched; $last = $now
        New-Item -ItemType Directory -Force -Path $CODEX_HOME | Out-Null
        Set-Content -Path $UPDATE_STATE -Value "last_check=$last`nlatest=$latest"
      }
    } catch { }   # fail-open
  }
  if (-not $latest) { return }
  if (-not (Test-VersionGt $latest $VERSION)) { return }
  if (Test-Interactive) {
    Write-Host -NoNewline "ByteAsk $latest is available (you have $VERSION). Update now? [Y/n] "
    $ans = Read-Host
    if ($ans -eq '' -or $ans -match '^[yY]') {   # Enter (default) or y -> update
      $gw = (Resolve-Gateway).TrimEnd('/')
      Write-Host "Updating ByteAsk to $latest ..."
      try {
        $env:PREFIX = $SELF_DIR
        Invoke-Expression (Invoke-RestMethod -Uri "$gw/install.ps1" -ErrorAction Stop)
        $env:BYTEASK_NO_UPDATE_CHECK = '1'
        & (Join-Path $SELF_DIR 'byteask.ps1') @script:LaunchArgs
        exit $LASTEXITCODE
      } catch { Write-Host "Update failed; continuing on $VERSION." }
    }
  } else {
    Write-Host "ByteAsk $latest is available (you have $VERSION). Run: byteask --update"
  }
}

# Test seam: `BYTEASK_PS_TEST=1` lets test/menu.tests.ps1 dot-source every
# function above WITHOUT running the launcher below (T27 adapter-mock tests).
if ($env:BYTEASK_PS_TEST) { return }

# ---- command dispatch (mirrors the sh case) --------------------------------
$script:LaunchArgs = @($args)
$cmd = if ($args.Count -gt 0) { "$($args[0])" } else { '' }

switch -Regex ($cmd) {
  '^(--version|-V|version)$' { Write-Host "byteask $VERSION"; exit 0 }
  '^(--update|update|upgrade)$' {
    Write-Host "Updating ByteAsk CLI..."
    $gw = (Resolve-Gateway).TrimEnd('/')
    $env:PREFIX = $SELF_DIR
    Invoke-Expression (Invoke-RestMethod -Uri "$gw/install.ps1")
    exit $LASTEXITCODE
  }
  '^login$' {
    $rest = @($args | Select-Object -Skip 1)
    if (($rest -contains '--with-api-key') -or ($rest -contains '--api-key')) {
      & $ENGINE login @rest; exit $LASTEXITCODE
    }
    $email = ''; $ref = ''
    for ($i = 0; $i -lt $rest.Count; $i++) {
      switch -Regex ($rest[$i]) {
        '^--email$'   { $email = $rest[++$i] }
        '^--gateway$' { $env:BYTEASK_GATEWAY = $rest[++$i] }
        '^--model$'   { $env:BYTEASK_MODEL = $rest[++$i] }
        '^--ref=(.*)$' { $ref = $Matches[1] }
      }
    }
    # `byteask login --email X` -> email flow + source menu; bare `byteask login` -> account menu.
    if ($rest.Count -gt 0) { Invoke-Login $email $ref; Show-SourceMenu } else { Invoke-Account }
    exit 0
  }
  '^logout$' { exit (Invoke-Logout) }
  '^byok$' {
    $rest = @($args | Select-Object -Skip 1)
    $sub = if ($rest.Count -ge 1) { $rest[0] } else { '' }
    Invoke-Byok $rest
    # set/remove reconfigure and want a fresh launch; everything else is terminal.
    if (@('set','remove','rm') -contains $sub) { $script:LaunchArgs = @() } else { exit 0 }
  }
  '^models$' {
    $rest = @($args | Select-Object -Skip 1)
    $mrc = Invoke-Models $rest
    if ($null -eq $mrc) { $mrc = 0 }
    exit ([int]$mrc)
  }
  '^terse$' {
    $rest = @($args | Select-Object -Skip 1)
    $trc = Invoke-Terse $rest
    if ($null -eq $trc) { $trc = 0 }
    exit ([int]$trc)
  }
  '^(--help|-h)$' {
    # ONE help text, mirroring cli/byteask (docs/terminal-surfaces-plan.md sec 5.8,
    # W-T5): the wrapper answers and never hands off to the engine's clap help,
    # which answered a second time in a second style and advertised upstream-only
    # subcommands incl. "Codex Cloud" (operating rule 4).
    # This file must stay PURE ASCII (PowerShell 5.1 reads a no-BOM .ps1 as cp1252
    # and an em-dash mis-decodes into a quote that terminates a string), so the
    # documented fallbacks apply: `-` for both the em-dash and the `.` separator.
    [Console]::Error.WriteLine("ByteAsk $VERSION - an AI coding agent for your terminal")
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine('  byteask                           start (signs you in on first run)')
    [Console]::Error.WriteLine('  byteask "<prompt>"                start with a prompt')
    [Console]::Error.WriteLine('  byteask exec "<prompt>"           one answer, no UI - scripts and CI')
    [Console]::Error.WriteLine('  byteask review                    review the working tree')
    [Console]::Error.WriteLine('  byteask login | logout            sign in or out')
    [Console]::Error.WriteLine('  byteask resume | fork             pick up an earlier session')
    [Console]::Error.WriteLine('  byteask apply                     apply the last diff')
    [Console]::Error.WriteLine('  byteask models <add|test|list>    use your own hosted model')
    [Console]::Error.WriteLine('  byteask byok <set|status|remove>  use your own provider key')
    [Console]::Error.WriteLine('  byteask mcp <add|list|remove>     manage MCP servers')
    [Console]::Error.WriteLine('  byteask plugin <list|install>     manage plugins')
    [Console]::Error.WriteLine('  byteask doctor                    check the install')
    [Console]::Error.WriteLine('  byteask completion <shell>        shell completions')
    [Console]::Error.WriteLine('  byteask --update                  update ByteAsk')
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine('Options: -m <model>  --effort <low|medium|high|xhigh>  -C <dir>')
    exit 0
  }
}

# Non-blocking, cached, fail-open update check before launching the engine.
Invoke-UpdateCheck

# Windows sandbox fallback. The engine sandboxes model-run shell commands with helper
# exes (codex-windows-sandbox-setup.exe + codex-command-runner.exe) that this build does
# NOT ship, so an enabled sandbox fails with "Windows cannot find
# codex-windows-sandbox-setup.exe". Run WITHOUT the engine sandbox unless the user set
# their own sandbox flag: --sandbox danger-full-access makes the engine skip
# get_platform_sandbox entirely (core/src/safety.rs), so no helper is ever spawned.
# Mirrors the Linux no-bwrap fallback; approval prompts still gate command execution.
$hasSandboxFlag = $false
foreach ($a in $script:LaunchArgs) {
  if ($a -in @('--sandbox','-s','--dangerously-bypass-approvals-and-sandbox','--full-auto')) { $hasSandboxFlag = $true; break }
}
if (-not $hasSandboxFlag) { $script:LaunchArgs = @('--sandbox','danger-full-access') + $script:LaunchArgs }

# Launch loop: run (not replace) the engine; on exit, act on any /login|/logout marker.
Remove-Item -Force -ErrorAction SilentlyContinue $AUTH_REQ
while ($true) {
  # A secret staged by the engine's in-TUI form is consumed by Invoke-RegisterModel in the
  # SAME iteration that stages it. Clear any leftover before handing control to the engine,
  # so a crash between staging and the hand-off can never leave an API key on disk into the
  # next session.
  Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $CODEX_HOME '.byteask-auth-secret')

  $cfg = Join-Path $CODEX_HOME 'config.toml'
  $prov = ''
  if (Test-Path $cfg) { $pm = Select-String -Path $cfg -Pattern '^model_provider = "(.*)"$' | Select-Object -First 1; if ($pm) { $prov = $pm.Matches[0].Groups[1].Value } }
  # Self-heal a managed config that lost its token line. The engine only ever sends
  # config.toml's experimental_bearer_token, but Get-CurrentJwt reads byok-config.json
  # first - so this state reports "signed in" everywhere and 401s on every turn, with no
  # way out of the settings screen. If a live JWT is still in the BYOK store, put it back;
  # otherwise fall through to sign-in below. No-op unless actually broken.
  if (Test-ManagedMissingToken) {
    # Heal only from a jwt that is actually LIVE. Gated on Test-SignedIn this would
    # restore an EXPIRED one and hand the engine a credential guaranteed to 401.
    $healJwt = Get-ByokField 'jwt'
    $healLeft = Get-JwtTtlLeft $healJwt
    if ($healJwt -and (($null -eq $healLeft) -or ($healLeft -gt 0))) {
      Write-ManagedConfig (Get-CfgLine '^model = "(.*)"$') (Get-CfgLine '^(model_catalog_json = .*)$') `
        ((Resolve-Gateway).TrimEnd('/')) $healJwt | Out-Null
    }
  }
  # Renew BEFORE the sign-in gate, so a session that is merely old repairs itself
  # instead of sending the user back through email.
  Invoke-SessionRefresh
  # Force sign-in only when the active config NEEDS a ByteAsk account (managed model,
  # un-keyed cloud model, or auto) and there's no valid session. A self-served config
  # (self/* or a cloud model with the user's own key) launches unsigned - its traffic
  # never touches us. If they later pick a managed model, the gateway's 401 asks then.
  if ((Test-NeedsSignin) -and ((-not (Test-SignedIn)) -or (Test-ManagedMissingToken))) {
    if (Test-Interactive) {
      Write-Host "Welcome to ByteAsk - let's get you signed in (one time)."
      Invoke-Account
    } else {
      Write-Err "You're not signed in to ByteAsk. Run:  byteask login --email you@company.com"
      exit 1
    }
  }

  # BYOK mode: ensure the loopback sidecar is up + current before launching.
  if ((Test-Path $cfg) -and (Select-String -Path $cfg -Pattern '^model_provider = "byok-local"' -Quiet)) {
    if (-not (Ensure-Sidecar)) { Write-Err "byteask: BYOK sidecar unavailable - own-key models may fail this run." }
  }

  # ripgrep: the model shells out to `rg` for code search. Without it the engine falls
  # back to slower methods and burns extra turns on large repos, with nothing on screen
  # explaining why. install.ps1 installs it best-effort, so this covers the case where
  # that failed. Warn once per process, never fatal; installing rg silences it.
  if ((-not $script:RgWarned) -and (-not (Get-Command rg -ErrorAction SilentlyContinue))) {
    $rgHint = "see https://github.com/BurntSushi/ripgrep#installation"
    if     (Get-Command winget -ErrorAction SilentlyContinue) { $rgHint = "winget install BurntSushi.ripgrep.MSVC" }
    elseif (Get-Command scoop  -ErrorAction SilentlyContinue) { $rgHint = "scoop install ripgrep" }
    elseif (Get-Command choco  -ErrorAction SilentlyContinue) { $rgHint = "choco install ripgrep" }
    Write-Err ("byteask: ripgrep (rg) not found - code search falls back to slower methods. Install it: " + $rgHint)
    $script:RgWarned = $true
  }

  # Self-hosted models: keep the /model catalog in sync with the registry before the
  # engine reads it (idempotent + atomic; survives --update). Gated so non-self users pay nothing.
  if (Test-HasSelfEndpoints) { Invoke-ModelsMerge }

  # No terminal + a prompt/args -> the interactive TUI fails with "stdin is not a terminal".
  # Point at `exec` (the headless one-shot mode) first. Only for a leading non-flag arg.
  if ([Console]::IsInputRedirected -and $script:LaunchArgs.Count -gt 0 -and $script:LaunchArgs[0] -ne 'exec' -and $script:LaunchArgs[0] -notlike '-*') {
    Write-Err "byteask: no terminal detected - the interactive UI needs one."
    Write-Err ("  For non-interactive / scripted use, run:  byteask exec """ + $script:LaunchArgs[0] + """")
  }

  & $ENGINE @script:LaunchArgs
  $rc = $LASTEXITCODE

  if (-not (Test-Path $AUTH_REQ)) { exit $rc }
  $act = (Get-Content -Raw $AUTH_REQ -ErrorAction SilentlyContinue)
  Remove-Item -Force -ErrorAction SilentlyContinue $AUTH_REQ
  # Line 1 is the verb (login/logout). Line 2, when present, is a headless instruction
  # from the engine's in-TUI settings screen: it already applied the change and only
  # needs us to finish, WITHOUT painting a second menu over the one the user was on.
  # The verb stays 'login' on purpose - this dispatch exits on anything it does not
  # recognise, so a newer engine paired with an older wrapper must still lead with a
  # known verb and fall through to the legacy screen rather than quitting.
  $detail = ''
  $actLines = ("$act" -split "`r?`n")
  if ($actLines.Count -ge 2) { $detail = "$($actLines[1])".Trim() }
  if ($act -match '^\s*logout') { exit (Invoke-Logout) }
  elseif ($act -match '^\s*login') {
    if ($detail -eq 'relaunch') { }                                  # already applied in-TUI
    # Only the linear magic-link wizard - NOT Invoke-Account, whose first move for a
    # signed-in user is the old settings screen, i.e. the menu the in-TUI surface
    # replaces. NB divergence from the sh wrapper: Invoke-Login calls `exit 1` on
    # failure and PowerShell cannot contain that the way a sh subshell can, so a
    # failed sign-in drops to the shell here instead of relaunching unsigned. That
    # matches what Invoke-Account already does today on this path.
    elseif ($detail -eq 'signin') { Invoke-Login '' '' }
    # Email already typed into the in-TUI form: pass it as a HINT so a refused address
    # re-prompts here instead of exiting the way an explicit --email must.
    elseif ($detail -like 'signin *') { Invoke-Login ($detail.Substring(7)).Trim() '' $true }
    elseif ($detail -like 'register-model *') { Invoke-RegisterModel ($detail.Substring(15)) }
    elseif ($detail -like 'add-model *') { Invoke-AddModel ($detail.Substring(10)) }
    elseif ($detail -like 'remove-model *') { Remove-Model @($detail.Substring(13)) }
    else { Invoke-Account }                                          # older engine: legacy menu
    $script:LaunchArgs = @(); continue
  }
  else { exit $rc }
}
