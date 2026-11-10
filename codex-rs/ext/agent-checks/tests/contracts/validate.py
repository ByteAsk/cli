#!/usr/bin/env python3
"""Validate a Cloud Agent contract message against its JSON Schema (stdlib only).

Usage:  python3 validate.py <schema-name> <file.json|file.toml>
        e.g. python3 validate.py task fixtures/valid/task.slack.json

Library: from validate import validate; errors = validate("task", doc)

Supports only the JSON Schema keywords listed in SUPPORTED. A schema that uses any other keyword is an
error, so nothing is ever silently ignored. Patterns are anchored in the schemas themselves and matched with
re.search, as JSON Schema specifies. Cross-field rules that JSON Schema cannot express live in SEMANTIC.
"""
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SCHEMA_DIR = os.path.join(HERE, "schemas")

SUPPORTED = {
    "$schema", "$id", "$defs", "$ref", "$comment", "title", "description",
    "type", "properties", "required", "additionalProperties", "enum", "const",
    "pattern", "minLength", "maxLength", "minimum", "maximum", "minItems", "maxItems",
    "items", "oneOf",
}

_cache = {}


class SchemaError(Exception):
    """The schema itself is broken (unknown keyword, bad $ref)."""


def load_schema(name):
    if name not in _cache:
        path = os.path.join(SCHEMA_DIR, name if name.endswith(".schema.json") else name + ".schema.json")
        with open(path, encoding="utf-8") as f:
            _cache[name] = json.load(f)
    return _cache[name]


def schema_names():
    return sorted(f[: -len(".schema.json")] for f in os.listdir(SCHEMA_DIR) if f.endswith(".schema.json"))


def _json_type(v):
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, int):
        return "integer"
    if isinstance(v, float):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    if isinstance(v, dict):
        return "object"
    return type(v).__name__


def _type_ok(v, t):
    jt = _json_type(v)
    return jt == t or (t == "number" and jt == "integer")


def _same(a, b):
    # JSON equality: true is not 1, 1 is 1.0
    if isinstance(a, bool) or isinstance(b, bool):
        return isinstance(a, bool) and isinstance(b, bool) and a == b
    return a == b


def _resolve(ref, base):
    file_part, _, pointer = ref.partition("#")
    doc_name = file_part[: -len(".schema.json")] if file_part else base
    doc = load_schema(doc_name)
    node = doc
    for part in [p for p in pointer.split("/") if p]:
        if not isinstance(node, dict) or part not in node:
            raise SchemaError(f"unresolvable $ref {ref!r} from {base}")
        node = node[part]
    return node, doc_name


def _check(inst, schema, path, base, errors):
    unknown = set(schema) - SUPPORTED
    if unknown:
        raise SchemaError(f"{base}: unsupported keyword(s) {sorted(unknown)} at {path or '/'}")
    if "$ref" in schema:
        target, new_base = _resolve(schema["$ref"], base)
        _check(inst, target, path, new_base, errors)
        return
    where = path or "/"
    t = schema.get("type")
    if t is not None:
        types = t if isinstance(t, list) else [t]
        if not any(_type_ok(inst, x) for x in types):
            errors.append(f"{where}: expected {'|'.join(types)}, got {_json_type(inst)}")
            return
    if "const" in schema and not _same(inst, schema["const"]):
        errors.append(f"{where}: must equal {schema['const']!r}")
    if "enum" in schema and not any(_same(inst, e) for e in schema["enum"]):
        errors.append(f"{where}: {inst!r} not in {schema['enum']}")
    if isinstance(inst, str):
        if "minLength" in schema and len(inst) < schema["minLength"]:
            errors.append(f"{where}: shorter than {schema['minLength']}")
        if "maxLength" in schema and len(inst) > schema["maxLength"]:
            errors.append(f"{where}: longer than {schema['maxLength']}")
        if "pattern" in schema and not re.search(schema["pattern"], inst):
            shown = inst if len(inst) <= 80 else inst[:77] + "..."
            errors.append(f"{where}: {shown!r} does not match {schema['pattern']}")
    if _json_type(inst) in ("integer", "number"):
        if "minimum" in schema and inst < schema["minimum"]:
            errors.append(f"{where}: below minimum {schema['minimum']}")
        if "maximum" in schema and inst > schema["maximum"]:
            errors.append(f"{where}: above maximum {schema['maximum']}")
    if isinstance(inst, list):
        if "minItems" in schema and len(inst) < schema["minItems"]:
            errors.append(f"{where}: fewer than {schema['minItems']} items")
        if "maxItems" in schema and len(inst) > schema["maxItems"]:
            errors.append(f"{where}: more than {schema['maxItems']} items")
        if "items" in schema:
            for i, item in enumerate(inst):
                _check(item, schema["items"], f"{path}/{i}", base, errors)
    if isinstance(inst, dict):
        for key in schema.get("required", []):
            if key not in inst:
                errors.append(f"{where}: missing required property {key!r}")
        props = schema.get("properties", {})
        extra = schema.get("additionalProperties", True)
        for key, val in inst.items():
            if key in props:
                _check(val, props[key], f"{path}/{key}", base, errors)
            elif extra is False:
                errors.append(f"{where}: unexpected property {key!r}")
            elif isinstance(extra, dict):
                _check(val, extra, f"{path}/{key}", base, errors)
    if "oneOf" in schema:
        matches = 0
        detail = []
        for i, sub in enumerate(schema["oneOf"]):
            sub_errors = []
            _check(inst, sub, path, base, sub_errors)
            if sub_errors:
                detail.append(f"[{i}] {sub_errors[0]}")
            else:
                matches += 1
        if matches != 1:
            errors.append(f"{where}: must match exactly one of {len(schema['oneOf'])} shapes, matched {matches}"
                          + (f" ({'; '.join(detail)})" if matches == 0 else ""))


# ---- cross-field rules JSON Schema cannot express ------------------------------------------------------

def _sem_task(doc):
    errs = []
    lim = doc.get("limits", {})
    if isinstance(lim.get("agent_s"), int) and isinstance(lim.get("deadline_s"), int):
        if lim["agent_s"] >= lim["deadline_s"]:
            errs.append("/limits: agent_s must be less than deadline_s (setup and verify need time)")
    return errs


def _sem_result(doc):
    errs = []
    status = doc.get("status")
    ev = doc.get("evidence", {}) or {}
    if status == "ok":
        if not doc.get("patch_b64"):
            errs.append("/: status ok needs patch_b64")
        if not doc.get("tree_sha"):
            errs.append("/: status ok needs tree_sha")
        verify = ev.get("verify")
        if not verify:
            errs.append("/evidence: status ok needs verify checks")
        elif verify.get("repo", {}).get("tree_sha") != doc.get("tree_sha"):
            errs.append("/evidence/verify: repo.tree_sha must equal the result's tree_sha")
        if verify and not verify.get("summary", {}).get("build_ok"):
            errs.append("/evidence/verify: status ok needs summary.build_ok true")
    if status == "no_change" and doc.get("patch_b64"):
        errs.append("/: status no_change must not carry a patch")
    return errs


def _sem_checks(doc):
    errs = []
    ids = [c.get("id") for c in doc.get("commands", [])]
    if len(ids) != len(set(ids)):
        errs.append("/commands: ids must be unique")
    known = set(ids)
    for i, t in enumerate(doc.get("tests", [])):
        if t.get("command_id") not in known:
            errs.append(f"/tests/{i}: command_id {t.get('command_id')!r} is not a command id")
    s = doc.get("summary", {})
    tests = doc.get("tests", [])
    if tests and isinstance(s.get("tests_total"), int) and s["tests_total"] != len(tests):
        errs.append("/summary: tests_total must equal the number of test entries")
    failed = sum(1 for t in tests if t.get("status") in ("fail", "error"))
    if tests and isinstance(s.get("tests_failed"), int) and s["tests_failed"] != failed:
        errs.append("/summary: tests_failed must count fail + error entries")
    return errs


SEMANTIC = {"task": _sem_task, "runner-result": _sem_result, "checks": _sem_checks}


def validate(schema_name, doc):
    """Return a list of error strings ([] means valid). Raises SchemaError for a broken schema."""
    errors = []
    _check(doc, load_schema(schema_name), "", schema_name, errors)
    if not errors and schema_name in SEMANTIC:
        errors.extend(SEMANTIC[schema_name](doc))
    # nested messages carry their own rules
    if not errors and schema_name == "runner-claim-response":
        errors.extend(_sem_task(doc.get("task", {})))
    if not errors and schema_name == "runner-result":
        for k, v in (doc.get("evidence") or {}).items():
            if v:
                errors.extend(f"/evidence/{k}{e}" for e in _sem_checks(v))
    return errors


def load_file(path):
    if path.endswith(".toml"):
        import tomllib
        with open(path, "rb") as f:
            return tomllib.load(f)
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def main(argv):
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    errors = validate(argv[1], load_file(argv[2]))
    for e in errors:
        print(e)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
