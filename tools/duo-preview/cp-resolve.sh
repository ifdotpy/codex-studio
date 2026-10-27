# Resolve preview-irrelevant conflicts: tests keep ours; non-web files take theirs.
resolve_easy() {
  local real=0
  for f in $(git diff --name-only --diff-filter=U); do
    case $f in
      *.spec.ts|*.test.ts|*.spec.mjs|*.test.mjs|*.test.tsx) git checkout --ours -- "$f" 2>/dev/null || git rm -q --cached "$f"; git add "$f" 2>/dev/null;;
      workspaces/client/apps/web/*) echo "REAL $f"; real=1;;
      *) git checkout --theirs -- "$f" 2>/dev/null; git add "$f" 2>/dev/null || git rm -q "$f";;
    esac
  done
  [ $real = 0 ] && git cherry-pick --quit 2>/dev/null
  return $real
}
pick_all() {
  for c in "$@"; do
    if git cherry-pick -n "$c" >/tmp/cp.log 2>&1; then echo "ok $c"; else
      echo "CONFLICT $c: $(git diff --name-only --diff-filter=U | tr '\n' ' ')"
      resolve_easy || { echo "STOP at $c"; return 1; }
      echo "  easy-resolved"
    fi
  done
}
