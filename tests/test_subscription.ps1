# Regression test for ChatGPT subscription model handling in byteask.ps1
# Issue: https://github.com/ByteAsk/cli/issues/3

$ErrorActionPreference = 'Stop'
$env:BYTEASK_PS_TEST = '1'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoDir = Split-Path -Parent $scriptDir
$byteaskPs1 = Join-Path $repoDir 'byteask.ps1'

$testHome = Join-Path ([System.IO.Path]::GetTempPath()) ("byteask-sub-test-" + [System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $testHome | Out-Null
$env:BYTEASK_HOME = $testHome

try {
  . $byteaskPs1

  $gw = 'https://code.byteask.ai'
  $jwt = 'test-token'

  # Test 1: Empty model in Write-SubscriptionConfig must omit model line
  $ok = Write-SubscriptionConfig '' '' $gw $jwt
  if (-not $ok) { throw "Write-SubscriptionConfig returned false" }

  $cfgPath = Join-Path $testHome 'config.toml'
  $cfg = Get-Content -Raw $cfgPath

  if ($cfg -match 'model\s*=\s*""') {
    throw "FAIL: model line written with empty string: model = `"`""
  }
  if ($cfg -match '^model\s*=') {
    throw "FAIL: model line should be absent in subscription config"
  }
  if (-not ($cfg -match '^model_provider = "openai"')) {
    throw "FAIL: model_provider should be openai"
  }

  # Test 2: Custom model in Write-SubscriptionConfig is respected
  $ok = Write-SubscriptionConfig 'gpt-5.6-terra' '' $gw $jwt
  if (-not $ok) { throw "Write-SubscriptionConfig custom model returned false" }
  $cfg = Get-Content -Raw $cfgPath
  if (-not ($cfg -match '^model = "gpt-5.6-terra"')) {
    throw "FAIL: expected model = `"gpt-5.6-terra`", got:`n$cfg"
  }

  # Test 3: Returning to managed via Invoke-ByokOff restores model_catalog_json
  $catPath = Join-Path $testHome 'models-catalog.json'
  Set-Content -Path $catPath -Value '{"models":[]}'
  # Current config is subscription (no model_catalog_json)
  $ok = Write-SubscriptionConfig '' '' $gw $jwt
  Invoke-ByokOff
  $cfg = Get-Content -Raw $cfgPath
  if (-not ($cfg -match 'model_catalog_json = ')) {
    throw "FAIL: Invoke-ByokOff failed to restore model_catalog_json"
  }

  Write-Host "All subscription tests passed."
}
finally {
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $testHome
}
