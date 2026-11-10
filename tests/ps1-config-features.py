#!/usr/bin/env python3
"""byteask.ps1's config writers keep the user's [features] settings, run under REAL PowerShell.
Mirrors config-features.sh. Needs a `pwsh`
(BYTEASK_PWSH or PATH); PowerShell 7 on Linux is not Windows PowerShell 5.1, so this proves
the logic, not the platform. PS1=<path> runs it against another copy of the launcher.

    BYTEASK_PWSH=/path/to/pwsh python3 tests/ps1-config-features.py
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
PS1 = os.environ.get("PS1") or os.path.join(HERE, "..", "byteask.ps1")
PWSH = os.environ.get("BYTEASK_PWSH") or shutil.which("pwsh")
RESULTS = []

DRIVER = r'''
param($Ps1, $HomeDir, $Writer)
$ErrorActionPreference = 'Stop'
$t = $null; $e = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Ps1, [ref]$t, [ref]$e)
$want = @('Get-ConfigFeaturesBlock', 'Save-ConfigToml', 'Write-ConfigWriteFailure',
          'Write-ManagedConfig', 'Write-ByokConfig', 'Write-SubscriptionConfig', 'Add-TersePref')
$funcs = $ast.FindAll({ $args[0] -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
foreach ($f in $funcs) { if ($want -contains $f.Name) { . ([scriptblock]::Create($f.Extent.Text)) } }
$BYTEASK_HOME = $HomeDir
$BYOK_PORT = 18923
function Write-Err([string]$m) { [Console]::Error.WriteLine($m) }
$cat = 'model_catalog_json = "/x/models-catalog.json"'
$r = switch ($Writer) {
  'signin'   { Write-ManagedConfig 'gpt-5.4' $cat 'https://gw.test' 'tok1' }
  'signout'  { Write-ManagedConfig 'gpt-5.4' $cat 'https://gw.test' '' }
  'byok'     { Write-ByokConfig 'gpt-5.4' $cat 'lt1' }
  'sub'      { Write-SubscriptionConfig '' $cat 'https://gw.test' 'jwt1' }
  'sub_signedout' { Write-SubscriptionConfig '' $cat 'https://gw.test' '' }
}
Write-Output ("ok=" + $r)
'''

SEEDS = {
    "TABLE": ('model = "gpt-5.4"\nmodel_provider = "byteask"\n\n[model_providers.byteask]\nname = "ByteAsk"\n\n'
              '[features]\ncommit_attribution = false\nplugins = true\n\n[projects."/tmp/x"]\ntrust_level = "trusted"\n',
              {"commit_attribution": False, "plugins": True}),
    "DOTTED": ('model = "gpt-5.4"\nfeatures.commit_attribution = false\nfeatures.plugins = true\n\n'
               '[model_providers.byteask]\nname = "ByteAsk"\n',
               {"commit_attribution": False, "plugins": True}),
    "NESTED": ('model = "gpt-5.4"\n\n[features]\n# a comment\ncommit_attribution = false\n\n'
               '[features.multi_agent_v2]\nenabled = true\n\n[model_providers.byteask]\nname = "ByteAsk"\n',
               {"commit_attribution": False, "multi_agent_v2": {"enabled": True}}),
    "NONE": ('model = "gpt-5.4"\n\n[model_providers.byteask]\nname = "ByteAsk"\n', None),
}


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    print("  %s %s%s" % ("PASS" if ok else "FAIL", name, ("  -- " + str(detail)[:300]) if detail and not ok else ""))


def run(home, writer):
    driver = os.path.join(home, "driver.ps1")
    open(driver, "w").write(DRIVER)
    p = subprocess.run([PWSH, "-NoLogo", "-NoProfile", "-File", driver, PS1, home, writer],
                       capture_output=True, text=True, timeout=120)
    return p.returncode, p.stdout + p.stderr


def features(cfg):
    try:
        with open(cfg, "rb") as f:
            return tomllib.load(f).get("features")
    except Exception as e:  # noqa: BLE001 - an unparsable file is the failure we report
        return "ERR %s" % e


def main():
    if not PWSH:
        print("SKIP: no pwsh (set BYTEASK_PWSH)")
        return 0
    home = tempfile.mkdtemp(prefix="bk-ps1-feat-")
    cfg = os.path.join(home, "config.toml")
    for writer in ("signin", "signout", "byok", "sub", "sub_signedout"):
        for seed, (text, want) in SEEDS.items():
            open(cfg, "w").write(text)
            rc, out = run(home, writer)
            got = features(cfg)
            check("%s keeps %s features" % (writer, seed), rc == 0 and "ok=True" in out and got == want,
                  (rc, got, out[-200:]))
            rc, out = run(home, writer)
            body = open(cfg).read()
            n = sum(1 for line in body.splitlines() if line.strip() == "[features]")
            check("%s/%s stable over a second rewrite" % (writer, seed), features(cfg) == want and n <= 1, (features(cfg), n))
    open(cfg, "w").write(SEEDS["TABLE"][0])
    run(home, "signin")
    body = open(cfg).read()
    check("sign-in token landed", 'experimental_bearer_token = "tok1"' in body, body[-300:])
    check("only [features] is carried", "trust_level" not in body, body)
    os.remove(cfg)
    rc, out = run(home, "signin")
    check("no previous config: works, no features", rc == 0 and features(cfg) is None, out[-200:])
    shutil.rmtree(home, ignore_errors=True)
    bad = [n for n, ok in RESULTS if not ok]
    print("\n%d checks, %d failed%s" % (len(RESULTS), len(bad), (": " + "; ".join(bad)) if bad else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
