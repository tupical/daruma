#!/usr/bin/env bash
# Import boundary guards run by .github/workflows/audit-imports.yml.
#
# Usage: scripts/audit-imports.sh <core|embed> [repo-root]
#
# Patterns are POSIX ERE because grep runs with -E: write `+`, `(` and `|`
# bare. Escaped, they are literals and the guard silently matches nothing.
# scripts/audit-imports.test.sh pins both guards against fixtures.
set -euo pipefail

case "${1:-}" in
  core)
    # §3.4 W4.1 — the core layer (crates/{shared,domain,events,core,
    # storage,auth} and apps/server/src/) must not depend on any
    # app crate. Apps depend on core, never the reverse.
    PATTERN='use[[:space:]]+(daruma_desktop|daruma_cli|daruma_mcp_bin|daruma_web)::'
    SCOPE_PATHS=(
      crates/shared
      crates/domain
      crates/events
      crates/core
      crates/storage
      crates/auth
      apps/server/src
    )
    ERROR='Core code must not import from app crates. See docs/MODULE_CONTRACT.md.'
    OK='OK: no app-crate imports in the core layer.'
    ;;
  embed)
    # §3.4 W2.1 — apps/desktop is the canonical embed client and must
    # not import directly from internal runtime crates. Other embed
    # clients added in the future should follow the same rule.
    PATTERN='use[[:space:]]+(daruma_storage|daruma_events)::'
    SCOPE_PATHS=(apps/desktop/src)
    ERROR='Embed-mode clients must reach for the runtime via daruma_core::embed::*, not internal crates. See docs/MODULE_CONTRACT.md#embed-mode.'
    OK='OK: no direct runtime-internal imports in embed clients.'
    ;;
  *)
    echo "usage: $0 <core|embed> [repo-root]" >&2
    exit 2
    ;;
esac

cd "${2:-.}"

violations=0
for path in "${SCOPE_PATHS[@]}"; do
  [ -d "$path" ] || continue
  status=0
  grep -rEn --include='*.rs' "$PATTERN" "$path" || status=$?
  case "$status" in
    0) violations=$((violations + 1)) ;;
    1) ;;
    # A grep that could not run has checked nothing: never report OK.
    *)
      echo "::error::grep failed with status $status on $path."
      exit 2
      ;;
  esac
done
if [ "$violations" -gt 0 ]; then
  echo
  echo "::error::$ERROR"
  exit 1
fi
echo "$OK"
