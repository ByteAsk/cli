"""Index ONE CMake target with the ByteAsk context engine (byteask company trial).

Written into the installed context engine by the ByteAsk engine and run with the system
python3 (-I). Uses the context engine (bctx) as a library; prints ONE JSON object on stdout:

  {"ok": true, "setting": ..., "commit": ..., "tus": N, "seconds": S, "lines": {...},
   "status": {...}, "target": ..., "targets_in_closure": [...], "overlay": {...}}
  {"ok": false, "error": "<one line, plain words>", "fix": "<one line>"}

"One target" = the target and every first-party target it depends on: exactly what
`cmake --build --target <target>` compiles. Only those translation units are indexed.
"""

import argparse
import json
import os
import sys
import time


def _fail(error, fix):
    print(json.dumps({"ok": False, "error": error, "fix": fix}))
    sys.exit(0)


def _codemodel(build_dir):
    """Targets of the newest CMake File API reply: {id: {name, type, sources, artifacts, deps}}."""
    import glob
    reply = os.path.join(build_dir, ".cmake", "api", "v1", "reply")
    idx = sorted(glob.glob(os.path.join(reply, "index-*.json")))
    if not idx:
        return None, None
    index = json.load(open(idx[-1]))
    for obj in index.get("objects", []):
        if obj.get("kind") != "codemodel":
            continue
        cm = json.load(open(os.path.join(reply, obj["jsonFile"])))
        src_root, bld_root = cm["paths"]["source"], cm["paths"]["build"]
        cfgs = cm.get("configurations") or [{}]
        targets = {}
        for t in cfgs[0].get("targets", []):
            td = json.load(open(os.path.join(reply, t["jsonFile"])))
            srcs = []
            for s in td.get("sources", []):
                if s.get("compileGroupIndex") is None:
                    continue  # headers and other non-compiled sources
                p = s["path"]
                srcs.append(os.path.normpath(p if os.path.isabs(p) else os.path.join(src_root, p)))
            arts = [os.path.normpath(os.path.join(bld_root, a["path"])) for a in td.get("artifacts", [])]
            targets[td["id"]] = {"name": td["name"], "type": td["type"], "sources": srcs, "artifacts": arts,
                                 "deps": [d["id"] for d in td.get("dependencies", [])]}
        return targets, src_root
    return None, None


def _closure(targets, name):
    start = [tid for tid, t in targets.items() if t["name"] == name]
    if not start:
        return None
    seen, todo = set(), list(start)
    while todo:
        tid = todo.pop()
        if tid in seen or tid not in targets:
            continue
        seen.add(tid)
        todo.extend(targets[tid]["deps"])
    return [targets[t] for t in sorted(seen)]


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--partition", required=True)
    ap.add_argument("--user", required=True)
    ap.add_argument("--project-root", required=True)
    ap.add_argument("--project-name", required=True)
    ap.add_argument("--build-dir", required=True)
    ap.add_argument("--setting", required=True)
    ap.add_argument("--target", required=True)
    ap.add_argument("--workers", type=int, default=2)
    a = ap.parse_args(argv)

    from bctx import configs, indexer, overlay, query, report, store, walls

    targets, _ = _codemodel(a.build_dir)
    if targets is None:
        _fail("CMake left no File API reply in the build directory",
              "re-run `byteask company trial`; it configures the project again")
    closure = _closure(targets, a.target)
    if closure is None:
        names = sorted(t["name"] for t in targets.values() if t["type"] != "UTILITY")
        _fail(f"the project has no CMake target named '{a.target}'",
              "pass --target with one of: " + ", ".join(names[:20]))
    wanted = {s for t in closure for s in t["sources"]}
    if not wanted:
        _fail(f"target '{a.target}' compiles no C/C++ source files of its own",
              "pass --target with an executable or library target that has sources")

    db_all = os.path.join(a.build_dir, "compile_commands.json")
    try:
        entries = json.load(open(db_all))
    except (OSError, ValueError):
        _fail("CMake wrote no compile_commands.json", "re-run `byteask company trial`")

    def _abs(e):
        f = e.get("file", "")
        return os.path.normpath(f if os.path.isabs(f) else os.path.join(e.get("directory", ""), f))

    picked = [e for e in entries if _abs(e) in wanted]
    if not picked:
        _fail(f"no compile command matches target '{a.target}'", "re-run `byteask company trial`")
    trial_dir = os.path.join(a.build_dir, "byteask-trial")
    os.makedirs(trial_dir, exist_ok=True)
    db_one = os.path.join(trial_dir, "compile_commands.json")
    with open(db_one, "w") as f:
        json.dump(picked, f, indent=1)

    # The partition and its one reader (the person running the trial).
    data = os.path.realpath(a.data)
    os.makedirs(data, exist_ok=True)
    acl_path = os.path.join(data, "acl.json")
    acl = json.load(open(acl_path)) if os.path.exists(acl_path) else {
        "version": walls.ACL_VERSION, "partitions": {}, "groups": {}}
    part = acl["partitions"].setdefault(a.partition, {"readers": []})
    part["readers"] = sorted(set(part.get("readers", [])) | {a.user})
    os.makedirs(os.path.join(data, "partitions", a.partition), exist_ok=True)
    with open(acl_path + ".tmp", "w") as f:
        json.dump(acl, f, indent=1)
    os.replace(acl_path + ".tmp", acl_path)
    w = walls.Walls(data)
    conn = store.connect(w.db_path(a.partition))

    pid = configs.ensure_project(conn, a.project_root, a.project_name)
    artifacts = {"linked": [p for t in closure if t["type"] in ("EXECUTABLE", "SHARED_LIBRARY", "MODULE_LIBRARY")
                            for p in t["artifacts"]],
                 "archives": [p for t in closure if t["type"] == "STATIC_LIBRARY" for p in t["artifacts"]]}
    try:
        cid = configs.add(conn, pid, a.setting, "cmake", a.build_dir, db_one, artifacts=artifacts)
    except ValueError as e:
        _fail(str(e).splitlines()[0], "pass a different --build-type, or remove the trial index and run again")

    t0 = time.time()
    committed = "indexed"
    try:
        snap = indexer.index(conn, cid, workers=a.workers, incremental=True,
                             time_trace="all" if len(picked) <= 100 else "sample:10",
                             log=lambda m: print(m, file=sys.stderr))
    except Exception as e:  # bctx refuses a dirty checkout for the committed index
        if "clean checkout" not in str(e):
            _fail("indexing failed: " + (str(e).splitlines() or [type(e).__name__])[0][:300],
                  "run `byteask company trial` again; if it fails the same way, send this line to ByteAsk")
        last = store.latest_complete(conn, cid)
        if last is None:
            _fail("the project has uncommitted changes in tracked files, and the first index is built "
                  "from a commit",
                  "commit or stash your changes (or use a fresh clone), then run `byteask company trial` again")
        snap, committed = last["snapshot_id"], "kept"
    seconds = round(time.time() - t0, 1)
    # The worktree's own overlay: answers then say which layer they come from and how many
    # commits the index is behind.
    ov = overlay.Overlay(w.overlay_dir(a.partition), a.project_root)
    try:
        ov_state = ov.refresh(conn, cid, overlay.Policy(debounce_s=0.0, max_workers=a.workers),
                              compdb_path=db_one, log=lambda m: print(m, file=sys.stderr))
    except Exception as e:  # the committed index is still good without it
        ov_state = {"error": str(e).splitlines()[0] if str(e) else type(e).__name__}
    st = query.Index(conn).status()
    print(json.dumps({"ok": True, "setting": a.setting, "config_id": cid, "snapshot": snap,
                      "committed": committed, "compdb": db_one,
                      "tus": len(picked), "seconds": seconds, "target": a.target,
                      "targets_in_closure": sorted(t["name"] for t in closure),
                      "lines": report.lines_indexed(conn), "status": st, "overlay": ov_state},
                     default=str))


if __name__ == "__main__":
    _here = os.path.dirname(os.path.abspath(__file__))  # <install>/python
    _install = os.path.dirname(_here)
    sys.path.insert(0, _here)
    os.environ.setdefault("BCTX_LLVM_ROOT", os.path.join(_install, "llvm"))
    os.environ.setdefault("BCTX_FACTS_BIN", os.path.join(_install, "bin", "bctx-facts"))
    os.environ.setdefault("BCTX_SCIP_CLANG_BIN", os.path.join(_install, "bin", "scip-clang"))
    main()
