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
$gz = "$enginePath.gz"
function Get-Engine($base) {   # try <asset>.gz (gunzip) then bare <asset>; $true on success
  try {
    Invoke-WebRequest -Uri "$base/$asset.gz" -OutFile $gz -UseBasicParsing
    $in  = [System.IO.File]::OpenRead($gz)
    $out = [System.IO.File]::Create($enginePath)
    $dec = New-Object System.IO.Compression.GzipStream($in, [System.IO.Compression.CompressionMode]::Decompress)
    $dec.CopyTo($out); $dec.Dispose(); $out.Dispose(); $in.Dispose()
    Remove-Item -Force $gz
    return $true
  } catch {
    Remove-Item -Force -ErrorAction SilentlyContinue $gz
    try { Invoke-WebRequest -Uri "$base/$asset" -OutFile $enginePath -UseBasicParsing; return $true } catch { return $false }
  }
}
# GitHub Releases first, then the code.byteask.ai fallback (-or short-circuits).
$got = (Get-Engine $EngineUrl) -or (Get-Engine $Bundle)
if (-not $got) { Write-Error "No ByteAsk build for windows/x86_64 (tried $EngineUrl then $Bundle)."; exit 1 }

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
$cfg = @"
model = "$Model"
model_provider = "byteask"
web_search = "live"

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
