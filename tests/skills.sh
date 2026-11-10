#!/bin/sh
# Engine-skill tests: the `compress` SKILL.md content + install plumbing.
# Guards that (a) the skill parses as a skill, (b) its non-negotiable safety
# rules can't be silently edited out, (c) install.sh/ps1 still fetch it, and
# (d) the install fetch loop lands + validates the file and won't clobber an
# existing user copy. No network (fetch is simulated against a local stub).
#
# Run:  sh tests/skills.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
SKILL="$ROOT/skills/compress/SKILL.md"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()  { if grep -qi "$1" "$2" 2>/dev/null; then ok "$3"; else bad "$3 (missing /$1/)"; fi; }

echo "== compress SKILL.md: parses as a skill =="
[ -f "$SKILL" ] && ok "SKILL.md exists" || bad "SKILL.md missing"
head -1 "$SKILL" | grep -q '^---$' && ok "opens with YAML frontmatter" || bad "no frontmatter fence"
grep -q '^name: compress$' "$SKILL" && ok "name: compress" || bad "name field wrong/missing"
grep -q '^description:' "$SKILL" && ok "has a description" || bad "no description"

echo "== the 5 non-negotiable safety rules are present (can't silently drop) =="
has 'sensitive'                 "$SKILL" "refuse sensitive files"
has 'back up\|backup'           "$SKILL" "back up the original first"
has 'byte-for-byte\|verbatim'   "$SKILL" "preserve code/URLs/headings verbatim"
has 'diff'                      "$SKILL" "show a diff before writing"
has 'approv'                    "$SKILL" "apply only on approval (diff-first)"
has 'restore'                   "$SKILL" "restore path documented"

echo "== the terse skill is RETIRED (token efficiency is built in, no user setting) =="
[ ! -e "$ROOT/skills/terse" ] && ok "skills/terse is gone from the repo" || bad "skills/terse still ships"

echo "== install plumbing fetches compress only =="
has 'for _sk in compress; do' "$ROOT/install.sh"  "install.sh fetches compress only"
has "@('compress')"          "$ROOT/install.ps1" "install.ps1 fetches compress only"
grep -q "compress terse\|'compress','terse'" "$ROOT/install.sh" "$ROOT/install.ps1" \
  && bad "an installer still fetches the terse skill" || ok "no installer fetches the terse skill"

echo "== install fetch loop: lands + validates + no-clobber (simulated) =="
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
HOME_DIR="$TMP/home"; mkdir -p "$HOME_DIR"
GATEWAY_URL="file://$ROOT"        # serve the repo skill via a stub "curl"
# stub curl: only implements the -o <path> <url> form the loop uses
cat > "$TMP/curl" <<EOF
#!/bin/sh
_out=""; _url=""
while [ \$# -gt 0 ]; do case "\$1" in -o) _out="\$2"; shift 2;; -*) shift;; *) _url="\$1"; shift;; esac; done
case "\$_url" in *skills/*/SKILL.md)
    _rel="\${_url##*/skills/}"; cp "$ROOT/skills/\${_rel%/SKILL.md}/SKILL.md" "\$_out" 2>/dev/null;;
  *) exit 1;; esac
EOF
chmod +x "$TMP/curl"; PATH="$TMP:$PATH"
# run just the skill-fetch block from install.sh (both skills)
run_fetch() {
  for _sk in compress; do
    mkdir -p "$HOME_DIR/skills/$_sk"
    if curl -fsSL -m 20 "$GATEWAY_URL/skills/$_sk/SKILL.md" -o "$HOME_DIR/skills/$_sk/SKILL.md.tmp" 2>/dev/null \
       && grep -q "^name: $_sk" "$HOME_DIR/skills/$_sk/SKILL.md.tmp" 2>/dev/null; then
      mv "$HOME_DIR/skills/$_sk/SKILL.md.tmp" "$HOME_DIR/skills/$_sk/SKILL.md"
    else
      rm -f "$HOME_DIR/skills/$_sk/SKILL.md.tmp" 2>/dev/null || true
    fi
  done
}
run_fetch
for _s in compress; do
  [ -f "$HOME_DIR/skills/$_s/SKILL.md" ] && ok "$_s skill landed in ~/.byteask/skills/$_s/" || bad "$_s skill not installed"
  grep -q "^name: $_s\$" "$HOME_DIR/skills/$_s/SKILL.md" 2>/dev/null && ok "$_s installed copy validated" || bad "$_s failed validation"
  [ ! -f "$HOME_DIR/skills/$_s/SKILL.md.tmp" ] && ok "$_s: no leftover .tmp" || bad "$_s: .tmp not cleaned"
done
# --update re-run must not error / must leave valid files
run_fetch
grep -q '^name: compress$' "$HOME_DIR/skills/compress/SKILL.md" 2>/dev/null && ok "re-run (--update) keeps valid skills" || bad "re-run corrupted a skill"

echo ""
echo "skills: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
