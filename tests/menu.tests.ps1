# Persistent settings screen - ADAPTER-MOCK tests.
# Runs the ps1 state machine + geometry under a fully mocked console, so it
# works HEADLESS (real PS 5.1 on Windows, or pwsh anywhere) - no
# interactive console needed. The real-console visual pass stays a manual
# checklist; everything mechanical is asserted here.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tests\menu.tests.ps1
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$wrapper = Join-Path (Split-Path -Parent $here) 'byteask.ps1'
if (-not (Test-Path $wrapper)) { $wrapper = Join-Path (Split-Path -Parent $here) 'byteask.ps1' }

$env:BYTEASK_PS_TEST = '1'
$env:BYTEASK_HOME = Join-Path ([IO.Path]::GetTempPath()) ("byteask-menutest-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $env:BYTEASK_HOME | Out-Null
. $wrapper    # dot-source all functions; the seam skips the launcher

$script:PASS = 0; $script:FAILN = 0
function OK([string]$m)  { $script:PASS++;  Write-Host ("  ok  " + $m) }
function BAD([string]$m) { $script:FAILN++; Write-Host ("FAIL  " + $m) }
function Assert([bool]$cond, [string]$m) { if ($cond) { OK $m } else { BAD $m } }

# ---- mock console -----------------------------------------------------------
# Simulates a buffer with a bottom edge (scrolling): WriteRow with a newline at
# the bottom keeps CursorTop pinned (exactly the conhost behavior behind R2).
function New-MockIO([object[]]$Keys, [int]$Height = 40, [int]$Width = 100, [int]$Bottom = 39) {
  $state = @{
    Keys = [System.Collections.Queue]::new()
    Rows = [System.Collections.ArrayList]::new()   # every WriteRow call (text,color,noNL)
    SetPosCalls = [System.Collections.ArrayList]::new()
    Top = 0; Bottom = $Bottom; H = $Height; W = $Width
  }
  foreach ($k in $Keys) { $state.Keys.Enqueue($k) }
  $io = @{
    __state  = $state
    GetSize  = { ,@($state.H, $state.W) }.GetNewClosure()
    CursorTop= { $state.Top }.GetNewClosure()
    SetPos   = { param($r) [void]$state.SetPosCalls.Add($r); $state.Top = [Math]::Max(0, $r) }.GetNewClosure()
    WriteRow = { param($text, $color, $noNL)
                 [void]$state.Rows.Add(@($text, $color, [bool]$noNL))
                 if (-not $noNL) { if ($state.Top -lt $state.Bottom) { $state.Top++ } } }.GetNewClosure()
    ReadKey  = { if ($state.Keys.Count -gt 0) { $state.Keys.Dequeue() } else { throw 'eof' } }.GetNewClosure()
    KeyAvail = { $state.Keys.Count -gt 0 -and $state.Keys.Peek().Key -eq '__burst__' }.GetNewClosure()
    CtrlC    = { param($on) }.GetNewClosure()
    ReadSecret = { 'sk-mock' }.GetNewClosure()
    ReadLine   = { 'mockline' }.GetNewClosure()
  }
  return $io
}
function K([string]$key, [string]$ch = '', [int]$mods = 0) {
  [pscustomobject]@{ Key = $key; KeyChar = $ch; Modifiers = $mods }
}
function Rendered([hashtable]$io) { ($io.__state.Rows | ForEach-Object { $_[0] }) -join "`n" }

# Stub the data layer: these tests exercise the SCREEN, not the key store
# (byok flows are covered by the CLI suites + the sh PTY suite).
function Get-CurrentEmail { 'tester@example.com' }
function Get-ByokKeyVerbs { ,@('add', 'add', 'change') }   # gemini keyed
function Get-ByokKeyCount { 1 }
function Get-ModelsPython { $null }                        # selfhost list: none

Write-Host "== T27a: frame renders the spec rows (keys state) =="
$io = New-MockIO @(); Set-ScrIOMock $io
$script:ScrG = Get-ScrGlyphs
Get-ScrData; Enter-ScrKeys
$script:ScrMsg = 'Saved your Gemini key'; $script:ScrMsgK = 'ok'
$script:ScrDrawn = 0
Show-ScrFrame
$out = Rendered $io
Assert ($out -match 'ByteAsk . Settings')            'header title row'
Assert ($out -match 'tester@example.com')            'email row'
Assert ($out -match 'API keys')                      'breadcrumb'
Assert ($out -match '> +Done|>  Done|> Done')        'cursor defaults to Done (keys exist)'
Assert ($out -match 'Remove a key')                  'Remove visible when a key exists'
Assert ($out -match 'Saved your Gemini key')         'status line carries the result'
Assert ($out -match 'Enter select')                  'footer hints'
$last = $io.__state.Rows[$io.__state.Rows.Count - 1]
Assert ($last[2] -eq $true)                          'LAST row written -NoNewline'

Write-Host "== T27b: hidden Remove at 0 keys + first-run default cursor =="
function Get-ByokKeyVerbs { ,@('add', 'add', 'add') }
function Get-ByokKeyCount { 0 }
$io = New-MockIO @(); Set-ScrIOMock $io
Get-ScrData; Enter-ScrKeys
$script:ScrMsg = ''; $script:ScrMsgK = 'none'; $script:ScrDrawn = 0
Show-ScrFrame
$out = Rendered $io
Assert (-not ($out -match 'Remove a key'))           'Remove HIDDEN with no keys'
Assert ($out -match '> Use ByteAsk managed|>  Use ByteAsk managed') 'first-run cursor on managed (bare Enter proceeds)'
Assert ($script:ScrOpts[0][0] -eq 'managed' -and $script:ScrCur -eq 1) 'managed is the FIRST row and holds the cursor'
Assert ($script:ScrOpts[0][1] -match '^Use ByteAsk managed \(recommended\)') 'managed row says (recommended)'

Write-Host "== T27c: relative-top geometry at the buffer BOTTOM =="
function Get-ByokKeyVerbs { ,@('add', 'add', 'change') }
function Get-ByokKeyCount { 1 }
$io = New-MockIO @() 40 100 39
$io.__state.Top = 39                                  # start AT the bottom edge
Set-ScrIOMock $io
Get-ScrData; Enter-ScrKeys
$script:ScrMsg = ''; $script:ScrMsgK = 'none'; $script:ScrDrawn = 0
Show-ScrFrame                                         # frame 1 (no SetPos: first draw)
$d1 = $script:ScrDrawn
Show-ScrFrame                                         # frame 2 repaints in place
Show-ScrFrame                                         # frame 3
$calls = $io.__state.SetPosCalls
Assert ($calls.Count -eq 2)                          'frames 2+3 repositioned (frame 1 drew fresh)'
Assert ($calls[0] -eq $calls[1])                     'region top STABLE across frames at buffer bottom'
Assert ($calls[0] -eq (39 - ($d1 - 1)))              'top = CursorTop - (drawn-1): relative math correct'

Write-Host "== T27d: input mapping - arrows/enter/esc; printables DEAD =="
$io = New-MockIO @((K 'UpArrow'), (K 'DownArrow'), (K 'Enter'), (K 'Escape'), (K 'D3' '3'), (K 'Q' 'q'))
Set-ScrIOMock $io
Assert ((Read-ScrKey) -eq 'up')    'UpArrow -> up'
Assert ((Read-ScrKey) -eq 'down')  'DownArrow -> down'
Assert ((Read-ScrKey) -eq 'enter') 'Enter -> enter'
Assert ((Read-ScrKey) -eq 'esc')   'Escape -> esc'
Assert ((Read-ScrKey) -eq 'none')  "printable '3' ignored (no jump-select)"
Assert ((Read-ScrKey) -eq 'none')  "printable 'q' ignored (no cancel)"

Write-Host "== T27e: Ctrl-C arrives as a KEY (TreatControlCAsInput path) =="
$io = New-MockIO @((K 'C' 'c' ([int][ConsoleModifiers]::Control)))
Set-ScrIOMock $io
Assert ((Read-ScrKey) -eq 'ctrlc') 'Ctrl+C -> ctrlc token'

Write-Host "== T27f: paste burst swallowed (printable followed by queued input) =="
$burst = @((K 'S' 's'))
foreach ($i in 1..5) { $burst += (K '__burst__' 'k') }
$io = New-MockIO $burst
Set-ScrIOMock $io
Assert ((Read-ScrKey) -eq 'paste') 'printable+burst -> paste'
Assert ($io.__state.Keys.Count -eq 0) 'burst fully drained'

Write-Host "== T27g: full loop - account -> keys -> esc -> account -> esc exits =="
function Test-ScrOk { $true }                         # bypass real-console gate for the mock
$io = New-MockIO @((K 'Enter'), (K 'Escape'), (K 'Escape'))
Set-ScrIOMock $io
$ran = Invoke-ScrSettings 'account'
$out = Rendered $io
Assert $ran                                          'screen loop ran to completion'
Assert ($out -match 'Manage keys & models')          'ACCOUNT frame rendered'
Assert ($out -match 'API keys')                      'Enter reached the KEYS frame'
Assert ($out -match 'ByteAsk settings')              'exit summary line printed'

Write-Host "== T27h: models dispatch exit codes + provider presets (parity) =="
# Exit-code contract (`models` must not always exit 0). These need no python.
Assert ((Invoke-Models @()) -eq 0)             'models (no sub) -> help, exit 0'
Assert ((Invoke-Models @('help')) -eq 0)       'models help -> exit 0'
Assert ((Invoke-Models @('bogus-sub')) -eq 2)  'unknown subcommand -> exit 2'
Assert ((Invoke-Models @('add')) -eq 1)        'add with no alias -> Add-Model $false -> exit 1'

# Preset paths need the real helper + python. Point BYOK_MODELS at the helpers/ copy and
# un-stub python for this block only (T27h is last, so no later test is affected).
$realHelper = Join-Path (Split-Path -Parent $here) 'helpers/byteask_models.py'
if (-not (Test-Path $realHelper)) { $realHelper = Join-Path (Split-Path -Parent $here) 'helpers\byteask_models.py' }
$pyExe = $null
foreach ($p in @('python3','python')) { if (Get-Command $p -ErrorAction SilentlyContinue) { $pyExe = $p; break } }
if ($pyExe -and (Test-Path $realHelper)) {
  $script:BYOK_MODELS = $realHelper
  $script:BYOK_CFG = Join-Path $env:BYTEASK_HOME 'byok-config.json'
  '{}' | Set-Content $script:BYOK_CFG
  function Get-ModelsPython { $pyExe }
  Assert (Test-PresetHelper)                    'helper version handshake reports >= 2'
  $prov = Get-ProviderPreset 'openrouter'
  Assert ($prov -and $prov.kind -eq 'cloud' -and $prov.wire -eq 'chat') 'preset openrouter resolves (cloud, wire=chat)'
  Assert ($null -eq (Get-ProviderPreset 'not-a-provider')) 'unknown preset id -> $null'
  $provOut = (& { Invoke-Models @('providers') } 6>&1 | Out-String)
  Assert ($provOut -match 'openrouter')         'models providers lists openrouter'
  # unknown --provider fails cleanly (returns $false -> exit 1), does not throw
  $rcBogus = 9; try { $rcBogus = Invoke-Models @('add','x','--provider','not-a-provider') } catch { $rcBogus = 99 }
  Assert ($rcBogus -eq 1)                        'add --provider <unknown> -> exit 1 (no crash)'
} else {
  Write-Host "  (no python on PATH - skipping preset-resolution asserts)"
}

Write-Host ""
Write-Host ("menu.tests.ps1: " + $script:PASS + " passed, " + $script:FAILN + " failed")
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $env:BYTEASK_HOME
if ($script:FAILN -gt 0) { exit 1 } else { exit 0 }
