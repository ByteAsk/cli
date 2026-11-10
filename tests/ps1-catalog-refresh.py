#!/usr/bin/env python3
"""byteask.ps1's model-access fetch + catalog refresh, run under REAL PowerShell against a
stub gateway. Mirrors catalog-refresh.sh. Needs a `pwsh` (BYTEASK_PWSH or PATH); PowerShell
7 on Linux is not Windows PowerShell 5.1, so this proves the logic, not the platform.

    BYTEASK_PWSH=/path/to/pwsh python3 tests/ps1-catalog-refresh.py
"""
import http.server
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
PS1 = os.environ.get("PS1") or os.path.join(HERE, "..", "byteask.ps1")
INSTALL_PS1 = os.path.join(HERE, "..", "install.ps1")
PWSH = os.environ.get("BYTEASK_PWSH") or shutil.which("pwsh")
STATE = {"version": "aaaaaaaaaaaa", "catalog": {"models": [{"slug": "served-model"}]}, "hits": [], "fail": False,
         "etag": '"6ab8dac0-e1490"', "inm": [], "auth": []}
RESULTS = []


class Stub(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        STATE["hits"].append(self.path)
        if STATE["fail"]:
            self.send_response(500); self.end_headers(); return
        if self.path == "/byteask/v1/model-access":
            STATE["auth"].append(self.headers.get("Authorization"))
            if not (self.headers.get("Authorization") or "").startswith("Bearer "):
                self.send_response(401); self.end_headers(); return
            body = json.dumps({"catalog_version": STATE["version"], "locked": [], "inactive": []}).encode()
        elif self.path == "/models-catalog.json":
            inm = self.headers.get("If-None-Match")
            STATE["inm"].append(inm)
            if inm and inm == STATE["etag"]:
                self.send_response(304); self.end_headers(); return
            c = STATE["catalog"]
            body = c if isinstance(c, bytes) else json.dumps(c).encode()
        else:
            self.send_response(404); self.end_headers(); return
        self.send_response(200)
        if self.path == "/models-catalog.json":
            self.send_header("ETag", STATE["etag"])
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


DRIVER = r'''
param($Ps1, $HomeDir, $Gateway, $Token, $Managed)
$ErrorActionPreference = 'Stop'
$t = $null; $e = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Ps1, [ref]$t, [ref]$e)
$want = @('Invoke-ModelsAccessFetch', 'Invoke-CatalogRefresh', 'Invoke-ModelsAccessMerge')
$funcs = $ast.FindAll({ $args[0] -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
foreach ($f in $funcs) { if ($want -contains $f.Name) { . ([scriptblock]::Create($f.Extent.Text)) } }
$BYTEASK_HOME = $HomeDir
$BYOK_MODELS = Join-Path $HomeDir 'no-such-helper.py'
function Resolve-Gateway { return $Gateway }
function Get-CurrentJwt { return $Token }
function Get-ManagedToken { return $Managed }
function Get-ModelsPython { return $null }
$script:MaFetched = $false
Invoke-ModelsAccessFetch
Invoke-CatalogRefresh
Invoke-ModelsAccessMerge
Write-Output ("fetched=" + $script:MaFetched)
'''


# A launch with NO ByteAsk sign-in: a conditional GET.
DRIVER_UNSIGNED = r'''
param($Ps1, $HomeDir, $Gateway, $Token)
$ErrorActionPreference = 'Stop'
$t = $null; $e = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Ps1, [ref]$t, [ref]$e)
$funcs = $ast.FindAll({ $args[0] -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
foreach ($f in $funcs) { if ($f.Name -eq 'Invoke-CatalogRefreshUnsigned') { . ([scriptblock]::Create($f.Extent.Text)) } }
$BYTEASK_HOME = $HomeDir
function Resolve-Gateway { return $Gateway }
function Get-CurrentJwt { return $Token }
Invoke-CatalogRefreshUnsigned
Write-Output "done"
'''


# install.ps1's install ping: the one top-level `if` that holds the ping URL.
DRIVER_PING = r'''
param($Ps1, $Bundle)
$ErrorActionPreference = 'Stop'
$t = $null; $e = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Ps1, [ref]$t, [ref]$e)
$st = @($ast.FindAll({ $args[0] -is [System.Management.Automation.Language.IfStatementAst] -and $args[0].Extent.Text -match 'byteask/dl/' }, $false))
if ($st.Count -ne 1) { throw "expected one guarded install ping, found $($st.Count)" }
. ([scriptblock]::Create($st[0].Extent.Text))
Write-Output "done"
'''


def run_ping(home, gateway, no_telemetry=None):
    driver = os.path.join(home, "driver-ping.ps1")
    open(driver, "w").write(DRIVER_PING)
    env = {k: v for k, v in os.environ.items() if k != "BYTEASK_NO_TELEMETRY"}
    if no_telemetry is not None:
        env["BYTEASK_NO_TELEMETRY"] = no_telemetry
    STATE["hits"].clear()
    p = subprocess.run([PWSH, "-NoLogo", "-NoProfile", "-File", driver, INSTALL_PS1, gateway],
                       capture_output=True, text=True, timeout=120, env=env)
    return p.returncode, p.stdout + p.stderr, list(STATE["hits"])


def run_unsigned(home, gateway, token=""):
    driver = os.path.join(home, "driver-unsigned.ps1")
    open(driver, "w").write(DRIVER_UNSIGNED)
    p = subprocess.run([PWSH, "-NoLogo", "-NoProfile", "-File", driver, PS1, home, gateway, token],
                       capture_output=True, text=True, timeout=120)
    return p.returncode, p.stdout + p.stderr


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    print("  %s %s%s" % ("PASS" if ok else "FAIL", name, ("  -- " + str(detail)[:300]) if detail and not ok else ""))


def run(home, gateway, token="tok", managed=""):
    driver = os.path.join(home, "driver.ps1")
    open(driver, "w").write(DRIVER)
    p = subprocess.run([PWSH, "-NoLogo", "-NoProfile", "-File", driver, PS1, home, gateway, token, managed],
                       capture_output=True, text=True, timeout=120)
    return p.returncode, p.stdout + p.stderr


def main():
    if not PWSH:
        print("SKIP: no pwsh (set BYTEASK_PWSH)")
        return 0
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    gw = "http://127.0.0.1:%d" % srv.server_address[1]
    home = tempfile.mkdtemp(prefix="bk-ps1-")
    cat = os.path.join(home, "models-catalog.json")
    local = {"models": [{"slug": "local-model"}]}
    read = lambda: json.load(open(cat))
    ver = lambda: open(os.path.join(home, ".catalog-version")).read() if os.path.exists(os.path.join(home, ".catalog-version")) else None

    json.dump(local, open(cat, "w"))
    rc, out = run(home, gw)
    check("the script runs without throwing", rc == 0 and "fetched=True" in out, out[-300:])
    check("a new server version replaces the catalog", read()["models"][0]["slug"] == "served-model", read())
    check("and records the version", ver() == "aaaaaaaaaaaa", ver())

    STATE["hits"].clear()
    rc, out = run(home, gw)
    check("the same version is not fetched again", "/models-catalog.json" not in STATE["hits"], STATE["hits"])

    STATE["version"] = "bbbbbbbbbbbb"
    STATE["catalog"] = b"<html>not json</html>"
    rc, out = run(home, gw)
    check("an invalid catalog never replaces the local one", rc == 0 and read()["models"][0]["slug"] == "served-model", (rc, out[-200:]))
    check("and the version is not advanced", ver() == "aaaaaaaaaaaa", ver())
    check("no temp file is left behind", not [f for f in os.listdir(home) if ".refresh." in f or f.endswith(".tmp")], os.listdir(home))

    STATE["catalog"] = {"models": []}
    rc, out = run(home, gw)
    check("an EMPTY model list is refused too", read()["models"][0]["slug"] == "served-model", read())

    STATE["fail"] = True
    rc, out = run(home, gw)
    check("a failing gateway changes nothing and does not throw", rc == 0 and "fetched=False" in out and read()["models"][0]["slug"] == "served-model", out[-200:])
    STATE["fail"] = False

    STATE["hits"].clear()
    rc, out = run(home, gw, token="")
    check("no token means no request at all", rc == 0 and not STATE["hits"], STATE["hits"])

    # The engine's token (config.toml) must win over a stale byok-config.json jwt.
    STATE["auth"].clear()
    rc, out = run(home, gw, token="stale-byok", managed="live-managed")
    check("the managed token wins over byok-config.json", STATE["auth"][-1:] == ["Bearer live-managed"], STATE["auth"])

    rc, out = run(home, "http://127.0.0.1:9")
    check("an unreachable gateway is silent", rc == 0 and "fetched=False" in out, out[-200:])

    # ---- unsigned launches: ETag conditional GET ----
    tagf = os.path.join(home, ".catalog-etag")
    tag = lambda: open(tagf).read() if os.path.exists(tagf) else None
    switch = lambda: (read().get("byteask_attribution") or {}).get("enabled")
    json.dump({"models": [{"slug": "unsigned-old"}]}, open(cat, "w"))
    STATE["catalog"] = {"models": [{"slug": "unsigned-new"}], "byteask_attribution": {"enabled": True}}
    STATE["hits"].clear()
    rc, out = run_unsigned(home, gw, token="tok")
    check("unsigned: a signed-in launch never takes this path", rc == 0 and not STATE["hits"], STATE["hits"])
    rc, out = run_unsigned(home, gw)
    check("unsigned: runs without throwing", rc == 0 and "done" in out, out[-300:])
    check("unsigned: catalog replaced, switch on", switch() is True, read())
    check("unsigned: ETag stored", tag() == STATE["etag"], tag())
    STATE["inm"].clear()
    rc, out = run_unsigned(home, gw)
    check("unsigned: sends If-None-Match", STATE["inm"] == [STATE["etag"]], STATE["inm"])
    check("unsigned: a 304 keeps the catalog and does not throw", rc == 0 and "done" in out and read()["models"][0]["slug"] == "unsigned-new", out[-300:])
    STATE["catalog"] = {"models": [{"slug": "unsigned-new"}], "byteask_attribution": {"enabled": False}}
    STATE["etag"] = '"6ab8dac0-ffff"'
    rc, out = run_unsigned(home, gw)
    check("unsigned: a served kill switch reaches the launch", switch() is False, read())
    check("unsigned: new ETag stored", tag() == '"6ab8dac0-ffff"', tag())
    STATE["catalog"] = b"<html>not json</html>"; STATE["etag"] = '"x1"'
    rc, out = run_unsigned(home, gw)
    check("unsigned: an invalid catalog is refused", rc == 0 and switch() is False, out[-200:])
    check("unsigned: ETag not advanced", tag() == '"6ab8dac0-ffff"', tag())
    STATE["fail"] = True
    rc, out = run_unsigned(home, gw)
    check("unsigned: a failing gateway changes nothing", rc == 0 and switch() is False, out[-200:])
    STATE["fail"] = False
    STATE["catalog"] = {"models": [{"slug": "odd"}]}; STATE["etag"] = "bad tag; x"
    rc, out = run_unsigned(home, gw)
    check("unsigned: an unsafe ETag is never stored", read()["models"][0]["slug"] == "odd" and tag() is None, (tag(), out[-200:]))
    check("unsigned: no temp file left", not [f for f in os.listdir(home) if ".refresh." in f], os.listdir(home))

    # ---- install.ps1: the install ping and its opt-out ----
    rc, out, hits = run_ping(home, gw)
    check("install ping: sent by default", rc == 0 and "done" in out and hits == ["/byteask/dl/windows-x86_64"], (out[-300:], hits))
    for v in ("1", "true", "TRUE", "Yes"):
        rc, out, hits = run_ping(home, gw, v)
        check("install ping: BYTEASK_NO_TELEMETRY=%s sends nothing" % v, rc == 0 and "done" in out and not hits, (out[-300:], hits))
    rc, out, hits = run_ping(home, gw, "0")
    check("install ping: BYTEASK_NO_TELEMETRY=0 does not opt out", rc == 0 and hits == ["/byteask/dl/windows-x86_64"], (out[-300:], hits))

    shutil.rmtree(home, ignore_errors=True)
    bad = [n for n, ok in RESULTS if not ok]
    print("\n%d checks, %d failed%s" % (len(RESULTS), len(bad), (": " + "; ".join(bad)) if bad else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
