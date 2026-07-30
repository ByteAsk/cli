# ByteAsk CLI installer for Windows.
#   irm https://code.byteask.ai/install.ps1 | iex
# Installs byteask-engine.exe + byteask.ps1 + a byteask.cmd shim, wires config, and
# adds the install dir to the user PATH. Runs under Windows PowerShell 5.1 (built in)
# or pwsh 7 - no extra runtime needed.
$ErrorActionPreference = 'Stop'

$Bundle  = if ($env:BUNDLE_URL) { $env:BUNDLE_URL.TrimEnd('/') } else { 'https://code.byteask.ai' }
# Engine binaries come from GitHub Releases (latest published release); code.byteask.ai is a fallback.
$EngineUrl = if ($env:ENGINE_URL) { $env:ENGINE_URL.TrimEnd('/') } else { 'https://github.com/ByteAsk/cli/releases/latest/download' }
$Gateway = if ($env:GATEWAY_URL) { $env:GATEWAY_URL.TrimEnd('/') } else { 'https://code.byteask.ai' }
$Model   = if ($env:MODEL) { $env:MODEL } else { 'gpt-5.4' }
$IsWin   = [System.Environment]::OSVersion.Platform -eq 'Win32NT'

if (-not [Environment]::Is64BitOperatingSystem) {
  Write-Error 'ByteAsk currently supports 64-bit Windows only.'; exit 1
}
$asset = 'byteask-engine-windows-x86_64.exe'

# Install dir: PREFIX env (used by in-place updates), else %LOCALAPPDATA%\Programs\ByteAsk.
$BinDir = if ($env:PREFIX) { $env:PREFIX } else { Join-Path $env:LOCALAPPDATA 'Programs\ByteAsk' }
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Write-Host "[byteask] downloading ByteAsk CLI (windows/x86_64)..."
$enginePath = Join-Path $BinDir 'byteask-engine.exe'
# D3a - the release marker. Records WHICH release asset produced the engine on disk:
# the asset name plus the sha256 of the COMPRESSED asset, i.e. exactly what
# SHA256SUMS makes a claim about. Skip-if-current compares this marker to the
# release manifest rather than hashing the installed binary, because the manifest
# covers the compressed assets while the installed file is the decompressed one -
# different artifacts - and because re-hashing a ~324 MB engine on every update is
# pure waste. A missing/garbled marker only ever costs one redundant download.
$marker = Join-Path $BinDir '.engine-release'
$script:EngineCurrent = $false   # $true = on-disk engine already matches the release
$script:EngineBadHash = $false   # $true = an asset was refused on a digest mismatch

# A downloaded engine must be a real, whole PE executable - not a proxy/block page or a
# truncated 200. The engine is hundreds of MB and starts with 'MZ', so a size floor +
# magic check reliably rejects junk that would otherwise "install" then fail to run.
function Test-Engine($path) {
  if (-not (Test-Path $path)) { return $false }
  if ((Get-Item $path).Length -lt 20000000) { return $false }
  try { $fs = [System.IO.File]::OpenRead($path); $b0 = $fs.ReadByte(); $b1 = $fs.ReadByte(); $fs.Dispose(); return ($b0 -eq 0x4D -and $b1 -eq 0x5A) } catch { return $false }
}
function Get-Sha256($path) {
  try { return (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLower() } catch { return '' }
}
# D3b - fetch the release manifest. Returns a name -> digest map, or $null when it
# is unreachable. See the policy note on Get-Engine below.
function Get-Manifest($base) {
  $tmp = Join-Path $BinDir '.byteask-sha256sums.tmp'
  try {
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $tmp -UseBasicParsing -TimeoutSec 60
    $map = @{}
    foreach ($line in (Get-Content $tmp)) {
      $parts = $line.Trim() -split '\s+', 2
      if ($parts.Count -eq 2) { $map[$parts[1].Trim().TrimStart('*')] = $parts[0].ToLower() }
    }
    Remove-Item -Force -ErrorAction SilentlyContinue $tmp
    if ($map.Count -eq 0) { return $null }
    return $map
  } catch { Remove-Item -Force -ErrorAction SilentlyContinue $tmp; return $null }
}
function Test-EngineCurrent($man) {
  if ($env:BYTEASK_FORCE_ENGINE_DOWNLOAD -eq '1') { return $false }
  if (-not $man) { return $false }
  if (-not (Test-Path $marker)) { return $false }
  if (-not (Test-Engine $enginePath)) { return $false }
  $a = ''; $h = ''
  foreach ($line in (Get-Content $marker)) {
    if ($line.StartsWith('asset='))  { $a = $line.Substring(6).Trim() }
    if ($line.StartsWith('sha256=')) { $h = $line.Substring(7).Trim() }
  }
  if (-not $a -or -not $h) { return $false }
  if (-not $man.ContainsKey($a)) { return $false }   # release doesn't list it -> can't claim current
  return ($man[$a] -eq $h)
}
# D3c - the compressor ladder. Measured on the real 324 MB linux-x86_64 engine:
# gzip-9 116.7 MB / 2.1 s to unpack, zstd-19 81.2 MB / 0.6 s, xz-6 75.1 MB / 4.8 s.
# .gz is published FOREVER so wrappers installed today never break, and .NET's
# GzipStream is built in - so on Windows .gz is always available, while .zst/.xz
# are used only when the user happens to have those tools on PATH.
$variants = @()
if (Get-Command zstd -ErrorAction SilentlyContinue) { $variants += 'zst' }
if (Get-Command xz   -ErrorAction SilentlyContinue) { $variants += 'xz' }
$variants += 'gz'
$variants += 'bare'
function Expand-Variant($v, $src, $dst) {
  # zstd/xz get an explicit output path - PowerShell's '>' mangles binary streams.
  try {
    switch ($v) {
      'zst'  { & zstd -d -q -f $src -o $dst | Out-Null; return ($LASTEXITCODE -eq 0) }
      'xz'   { & xz -d -k -f $src          | Out-Null; return ($LASTEXITCODE -eq 0) }
      'gz'   {
        $in  = [System.IO.File]::OpenRead($src)
        $out = [System.IO.File]::Create($dst)
        $dec = New-Object System.IO.Compression.GzipStream($in, [System.IO.Compression.CompressionMode]::Decompress)
        $dec.CopyTo($out); $dec.Dispose(); $out.Dispose(); $in.Dispose()
        return $true
      }
      'bare' { Move-Item -Force $src $dst; return $true }
    }
  } catch { return $false }
  return $false
}
# D3b policy: FAIL-CLOSED on a mismatch, FAIL-OPEN on an absent manifest.
#  * manifest lists the asset and the digest DISAGREES -> refuse it, delete it, drop
#    to the next rung. Never install a binary the release says is not what we asked for.
#  * manifest unreachable, or the asset is not listed -> install anyway, with a note,
#    after the existing size+magic check.
# Fail-open on absence is deliberate: the manifest and the asset travel the same TLS
# connection from the same host, so an adversary who can suppress the manifest can
# already rewrite the asset. The digest's real value is catching corruption -
# truncated transfers, caching proxies, mangled CDN objects - against which absence
# carries no signal. Failing closed would instead brick installs on every flaky or
# filtering network, break the code.byteask.ai fallback (which serves no SHA256SUMS)
# and break every already-published release. Set BYTEASK_REQUIRE_SHA256=1 to make
# absence fatal too.
function Get-Engine($base) {   # $true on success (including "already current")
  $man = Get-Manifest $base
  if (Test-EngineCurrent $man) { $script:EngineCurrent = $true; return $true }
  for ($try = 1; $try -le 3; $try++) {
    foreach ($v in $variants) {
      if ($v -eq 'bare') { $name = $asset; $dl = "$enginePath.dl" }
      else               { $name = "$asset.$v"; $dl = "$enginePath.$v" }
      Remove-Item -Force -ErrorAction SilentlyContinue $dl
      try { Invoke-WebRequest -Uri "$base/$name" -OutFile $dl -UseBasicParsing -TimeoutSec 600 }
      catch { Remove-Item -Force -ErrorAction SilentlyContinue $dl; continue }
      $want = ''
      if ($man -and $man.ContainsKey($name)) { $want = $man[$name] }
      $got = Get-Sha256 $dl
      if ($want -and $got -and ($want -ne $got)) {
        Write-Host "[byteask] REFUSING ${name}: sha256 mismatch against the release SHA256SUMS." -ForegroundColor Red
        Write-Host "            expected  $want" -ForegroundColor Red
        Write-Host "            got       $got" -ForegroundColor Red
        Write-Host "          The download is corrupt or was tampered with; it will NOT be installed." -ForegroundColor Red
        Remove-Item -Force -ErrorAction SilentlyContinue $dl
        $script:EngineBadHash = $true; continue
      }
      if (-not $want) {
        if ($env:BYTEASK_REQUIRE_SHA256 -eq '1') {
          Write-Host "[byteask] REFUSING ${name}: BYTEASK_REQUIRE_SHA256=1 and it has no SHA256SUMS entry." -ForegroundColor Red
          Remove-Item -Force -ErrorAction SilentlyContinue $dl
          $script:EngineBadHash = $true; continue
        }
        Write-Host "[byteask] note: no SHA256SUMS entry for $name - installing unverified (size+magic checked)."
      }
      $okExpand = Expand-Variant $v $dl $enginePath
      if ($okExpand -and (Test-Engine $enginePath)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $dl
        # Record what produced it, for the next update's skip-if-current.
        try { Set-Content -Path $marker -Value "asset=$name`nsha256=$got" -NoNewline } catch {}
        if ($want -and $got) { Write-Host "[byteask] fetched $name (sha256 verified)" }
        else                 { Write-Host "[byteask] fetched $name" }
        return $true
      }
      Remove-Item -Force -ErrorAction SilentlyContinue $dl
    }
    Start-Sleep -Seconds 2
  }
  return $false
}
# GitHub Releases first, then the code.byteask.ai fallback (-or short-circuits).
$got = (Get-Engine $EngineUrl) -or (Get-Engine $Bundle)
if (-not $got) {
  Write-Host ""
  if ($script:EngineBadHash) {
    Write-Host "[byteask] Every engine download for windows/x86_64 failed its checksum (or was" -ForegroundColor Red
    Write-Host "  unverifiable while BYTEASK_REQUIRE_SHA256=1). Nothing was installed." -ForegroundColor Red
    Write-Host "  This usually means a caching proxy or VPN is rewriting the download." -ForegroundColor Red
  } else {
    Write-Host "[byteask] Couldn't download the engine for windows/x86_64. The build IS published -" -ForegroundColor Red
    Write-Host "  this is a network issue on THIS machine (a proxy/firewall blocking GitHub or" -ForegroundColor Red
    Write-Host "  code.byteask.ai, no internet/DNS, or a VPN intercepting HTTPS)." -ForegroundColor Red
  }
  Write-Host "  Tried:  $EngineUrl/$asset.gz  and  $Bundle/$asset.gz" -ForegroundColor Red
  Write-Host "  Retry, fix the proxy, or install self-contained (no GitHub needed):" -ForegroundColor Red
  Write-Host "     pip install byteask     (or)     npm install -g @byteask/cli" -ForegroundColor Red
  exit 1
}
if ($script:EngineCurrent) {
  Write-Host "[byteask] engine already at this release - skipped the download."
}

# ripgrep (rg): the model uses it for fast code search. Without it, searches fall back to
# slower methods and the model wastes agent turns (and tokens). Best-effort via a package
# manager if one is present; never fatal - a missing rg only slows search.
if (-not (Get-Command rg -ErrorAction SilentlyContinue)) {
  Write-Host "[byteask] installing ripgrep (rg) for fast code search..."
  try {
    if     (Get-Command winget -ErrorAction SilentlyContinue) { winget install --id BurntSushi.ripgrep.MSVC -e --silent --accept-package-agreements --accept-source-agreements 2>$null | Out-Null }
    elseif (Get-Command scoop  -ErrorAction SilentlyContinue) { scoop install ripgrep 2>$null | Out-Null }
    elseif (Get-Command choco  -ErrorAction SilentlyContinue) { choco install ripgrep -y 2>$null | Out-Null }
  } catch {}
  # The try/catch above swallows failure, so "installing..." is not evidence it
  # worked (no package manager, or the install was blocked). Say so plainly rather
  # than leaving the user to wonder why search is slow later.
  if (-not (Get-Command rg -ErrorAction SilentlyContinue)) {
    Write-Host "[byteask] note: couldn't install ripgrep - code search will fall back to slower methods. Install it later: https://github.com/BurntSushi/ripgrep#installation"
  }
}

# Per-platform install beacon (fire-and-forget; never blocks or fails the run). The
# engine now downloads from GitHub, so the gateway can't observe it directly - this
# ping is the per-platform install/update signal. Best-effort, fail-open.
try { Invoke-WebRequest -Uri "$Bundle/byteask/dl/windows-x86_64" -UseBasicParsing -TimeoutSec 3 | Out-Null } catch {}

# Wrapper + a byteask.cmd shim so `byteask` works from any shell (uses built-in
# Windows PowerShell; -ExecutionPolicy Bypass avoids per-machine policy blocks).
Invoke-WebRequest -Uri "$Bundle/byteask.ps1" -OutFile (Join-Path $BinDir 'byteask.ps1') -UseBasicParsing
$cmd = "@echo off`r`npowershell -NoProfile -ExecutionPolicy Bypass -File `"%~dp0byteask.ps1`" %*`r`n"
Set-Content -Path (Join-Path $BinDir 'byteask.cmd') -Value $cmd -NoNewline

# Config home + gateway + optional one-shot referral.
$ByteHome = if ($env:BYTEASK_HOME) { $env:BYTEASK_HOME } else { Join-Path $HOME '.byteask' }
New-Item -ItemType Directory -Force -Path $ByteHome | Out-Null
Set-Content -Path (Join-Path $ByteHome 'gateway') -Value $Gateway -NoNewline
if ($env:BYTEASK_REF -and ($env:BYTEASK_REF -match '^[A-Za-z0-9_-]{1,64}$')) {
  Set-Content -Path (Join-Path $ByteHome 'referral') -Value $env:BYTEASK_REF -NoNewline
}

# Carry an existing login token across updates (config.toml is rewritten below).
$prevToken = ''
$cfgPath = Join-Path $ByteHome 'config.toml'
if (Test-Path $cfgPath) {
  $m = Select-String -Path $cfgPath -Pattern '^experimental_bearer_token = "(.*)"$'
  if ($m) { $prevToken = $m.Matches[0].Groups[1].Value }
}
$tokenLine = if ($prevToken) { "experimental_bearer_token = `"$prevToken`"" } else { '' }
# Model catalog: adds Claude (opus/sonnet) to /model with correct metadata (it
# REPLACES the engine's bundled catalog, so only reference it after the download
# validates). Fail-safe: skip on any failure (Claude still routes via the gateway).
$catalogLine = ''
$catPath = Join-Path $ByteHome 'models-catalog.json'
$catTmp = "$catPath.tmp"
try {
  Invoke-WebRequest -Uri "$Gateway/models-catalog.json" -OutFile $catTmp -TimeoutSec 20 -UseBasicParsing -ErrorAction Stop
  if ((Test-Path $catTmp) -and (Select-String -Path $catTmp -Pattern '"models"' -Quiet)) {
    Move-Item -Force $catTmp $catPath
    $catalogLine = 'model_catalog_json = "' + ($catPath -replace '\\','/') + '"'
  } else { Remove-Item -Force -ErrorAction SilentlyContinue $catTmp }
} catch { Remove-Item -Force -ErrorAction SilentlyContinue $catTmp }

# BYOK sidecar + translators (own-key routing). Fail-soft: a managed install is
# never affected if they aren't served yet. 'byteask --update' refreshes them.
# effort.py is a DEPENDENCY of the translators (they import it at module level),
# so it must land BEFORE them. The per-file fetch is fail-soft - a failure keeps
# the previous working copy - but effort.py has no previous copy on a first
# upgrade, so a translator updated without it is an ImportError that kills the
# whole sidecar. Gate the group on it.
$effortOk = $false
$eTmp = (Join-Path $ByteHome 'effort.py') + '.tmp'
try {
  Invoke-WebRequest -Uri "$Gateway/effort.py" -OutFile $eTmp -TimeoutSec 20 -UseBasicParsing -ErrorAction Stop
  if ((Test-Path $eTmp) -and (Select-String -Path $eTmp -Pattern 'def clamp_effort' -Quiet)) {
    Move-Item -Force $eTmp (Join-Path $ByteHome 'effort.py')
    $effortOk = $true
  } else { Remove-Item -Force -ErrorAction SilentlyContinue $eTmp }
} catch { Remove-Item -Force -ErrorAction SilentlyContinue $eTmp }
if (-not $effortOk -and (Test-Path (Join-Path $ByteHome 'effort.py'))) { $effortOk = $true }

if ($effortOk) {
foreach ($f in @('byteask_errors.py','byok_sidecar.py','anthropic_translate.py','gemini_translate.py','openai_compat_translate.py','byteask_models.py')) {
  $tmp = (Join-Path $ByteHome $f) + '.tmp'
  try {
    Invoke-WebRequest -Uri "$Gateway/$f" -OutFile $tmp -TimeoutSec 20 -UseBasicParsing -ErrorAction Stop
    if ((Test-Path $tmp) -and (Select-String -Path $tmp -Pattern 'def translate_request|PROTOCOL_VERSION|is_anthropic_model|is_gemini_model|SELF_BASE_INSTRUCTIONS|TERMINAL_SSE_CODE' -Quiet)) {
      Move-Item -Force $tmp (Join-Path $ByteHome $f)
    } else { Remove-Item -Force -ErrorAction SilentlyContinue $tmp }
  } catch { Remove-Item -Force -ErrorAction SilentlyContinue $tmp }
}
}

# Engine skills -> the user-skills dir the engine's /skills menu reads. No rebuild.
foreach ($sk in @('compress','terse')) {
  $skDir = Join-Path (Join-Path $ByteHome 'skills') $sk
  New-Item -ItemType Directory -Force -Path $skDir | Out-Null
  $skTmp = (Join-Path $skDir 'SKILL.md') + '.tmp'
  try {
    Invoke-WebRequest -Uri "$Gateway/skills/$sk/SKILL.md" -OutFile $skTmp -TimeoutSec 20 -UseBasicParsing -ErrorAction Stop
    if ((Test-Path $skTmp) -and (Select-String -Path $skTmp -Pattern "^name: $sk" -Quiet)) {
      Move-Item -Force $skTmp (Join-Path $skDir 'SKILL.md')
    } else { Remove-Item -Force -ErrorAction SilentlyContinue $skTmp }
  } catch { Remove-Item -Force -ErrorAction SilentlyContinue $skTmp }
}

$cfg = @"
model = "$Model"
model_provider = "byteask"
web_search = "live"
$catalogLine

[model_providers.byteask]
name = "ByteAsk"
base_url = "$Gateway/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
$tokenLine

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
"@
Set-Content -Path $cfgPath -Value $cfg

# Add BinDir to the user PATH (Windows only; registry-backed, persists).
$pathNote = ''
if ($IsWin) {
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not $userPath) { $userPath = '' }
  if (($userPath -split ';') -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$BinDir"), 'User')
    $pathNote = "Added $BinDir to your PATH - open a NEW terminal to pick it up."
  }
}

$ver = try { & $enginePath --version 2>$null } catch { 'byteask' }
Write-Host ""
Write-Host "  [OK] byteask installed  ->  $BinDir\byteask.cmd"
Write-Host ""
Write-Host "  To start, just run:"
Write-Host ""
Write-Host "      byteask"
Write-Host ""
Write-Host "  and you're in interactive mode - like  claude  or  codex."
if ($pathNote) { Write-Host ""; Write-Host "  $pathNote" }
Write-Host ""
Write-Host "  New here? Sign in first:  byteask login --email you@company.com"
Write-Host ""
