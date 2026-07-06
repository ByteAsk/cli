#!/usr/bin/env pwsh
# byteask.ps1 - ByteAsk AI coding agent CLI (native Windows launcher).
#
# PowerShell port of the POSIX `byteask` wrapper: same auth flow (magic-link, mode-C
# token in config.toml), same hourly re-prompting update check, same interactive
# onboarding, and the same /login|/logout launch loop (the TUI drops a marker; we
# re-auth here and relaunch, since the engine can't hot-swap the startup-loaded token).
#
# Behavior parity with byteask/cli/byteask is intentional - keep them in sync.

$VERSION = '0.1.3'
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
function Invoke-Logout {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  $had = $false
  if ((Test-Path $cfg) -and (Select-String -Path $cfg -Pattern 'experimental_bearer_token' -Quiet)) {
    $had = $true
    $kept = @(Get-Content $cfg | Where-Object { $_ -notmatch 'experimental_bearer_token' })
    Set-Content -Path $cfg -Value $kept
  }
  Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $CODEX_HOME 'auth.json')
  if (-not $had) { Write-Host "You're not signed in to ByteAsk."; return 0 }
  if (Select-String -Path $cfg -Pattern 'experimental_bearer_token' -Quiet) {
    Write-Err "Couldn't remove the saved token - check permissions on $cfg"; return 1
  }
  Write-Host "Logged out of ByteAsk. Run 'byteask' to sign back in."
  return 0
}

# Magic-link sign-in; writes gateway + config.toml with the mode-C token. Exits 1 on
# failure (like the sh wrapper). $Email/$Ref empty = prompt / discover.
function Invoke-Login([string]$Email, [string]$Ref) {
  $gateway = (Resolve-Gateway).TrimEnd('/')
  $model = if ($env:BYTEASK_MODEL) { $env:BYTEASK_MODEL } else { 'gpt-5.4' }
  if (-not $Ref) { $Ref = $env:BYTEASK_REF }
  $refFile = Join-Path $CODEX_HOME 'referral'
  if (-not $Ref -and (Test-Path $refFile)) { $Ref = (Get-Content -Raw $refFile).Trim() }
  if ($Ref -and (($Ref -notmatch '^[A-Za-z0-9_-]+$') -or ($Ref.Length -gt 64))) { $Ref = '' }
  if (-not $Email) { $Email = Read-Host 'Email' }

  Write-Host "Signing in to ByteAsk as $Email ..."
  $bodyObj = if ($Ref) { @{ email = $Email; ref = $Ref } } else { @{ email = $Email } }
  $body = $bodyObj | ConvertTo-Json -Compress
  try {
    $start = Invoke-RestMethod -Uri "$gateway/auth/start" -Method Post -Body $body -ContentType 'application/json' -ErrorAction Stop
  } catch { Write-Err "sign-in failed: $($_.Exception.Message)"; exit 1 }
  $poll = $start.poll_token
  $link = $start.dev_magic_link
  if (-not $poll) { Write-Err "sign-in failed"; exit 1 }
  Write-Host "  -> Check $Email for a sign-in link and click it."
  if ($link) {
    Write-Host "  -> (dev) link: $link"
    try { Invoke-RestMethod -Uri $link -TimeoutSec 5 -ErrorAction Stop | Out-Null; Write-Host "  -> sign-in confirmed" } catch { }
  }
  Write-Host "  -> Waiting for confirmation ..."
  $token = ''
  for ($i = 0; $i -lt 120; $i++) {
    try {
      $r = Invoke-RestMethod -Uri "$gateway/auth/poll" -Method Post -Body (@{ poll_token = $poll } | ConvertTo-Json -Compress) -ContentType 'application/json' -ErrorAction Stop
      if ($r.status -eq 'approved') { $token = $r.access_token; break }
    } catch { }
    Start-Sleep -Seconds 1
  }
  if (-not $token) { Write-Err "sign-in timed out"; exit 1 }
  New-Item -ItemType Directory -Force -Path $CODEX_HOME | Out-Null
  Set-Content -Path (Join-Path $CODEX_HOME 'gateway') -Value $gateway -NoNewline
  $cfg = @"
model = "$model"
model_provider = "byteask"
web_search = "live"

[model_providers.byteask]
name = "ByteAsk"
base_url = "$gateway/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "$token"

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
"@
  Set-Content -Path (Join-Path $CODEX_HOME 'config.toml') -Value $cfg
  Remove-Item -Force -ErrorAction SilentlyContinue $refFile     # one-shot referral
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
    Write-Host -NoNewline "ByteAsk $latest is available (you have $VERSION). Update now? [y/N] "
    $ans = Read-Host
    if ($ans -match '^[yY]') {
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
    Invoke-Login $email $ref
    exit 0
  }
  '^logout$' { exit (Invoke-Logout) }
}

# Non-blocking, cached, fail-open update check before launching the engine.
Invoke-UpdateCheck

# Launch loop: run (not replace) the engine; on exit, act on any /login|/logout marker.
Remove-Item -Force -ErrorAction SilentlyContinue $AUTH_REQ
while ($true) {
  $cfg = Join-Path $CODEX_HOME 'config.toml'
  $isByteask = (Test-Path $cfg) -and (Select-String -Path $cfg -Pattern '^model_provider = "byteask"' -Quiet)
  $hasToken  = (Test-Path $cfg) -and (Select-String -Path $cfg -Pattern 'experimental_bearer_token' -Quiet)
  if ($isByteask -and (-not $hasToken)) {
    if (Test-Interactive) {
      Write-Host "Welcome to ByteAsk - let's get you signed in (one time)."
      Invoke-Login '' ''
    } else {
      Write-Err "You're not signed in to ByteAsk. Run:  byteask login --email you@company.com"
      exit 1
    }
  }

  & $ENGINE @script:LaunchArgs
  $rc = $LASTEXITCODE

  if (-not (Test-Path $AUTH_REQ)) { exit $rc }
  $act = (Get-Content -Raw $AUTH_REQ -ErrorAction SilentlyContinue)
  Remove-Item -Force -ErrorAction SilentlyContinue $AUTH_REQ
  if ($act -match '^\s*logout') { exit (Invoke-Logout) }
  elseif ($act -match '^\s*login') { Invoke-Login '' ''; $script:LaunchArgs = @(); continue }
  else { exit $rc }
}
