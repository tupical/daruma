#!/usr/bin/env bash
# Regression for scripts/audit-imports.sh: every case builds a throwaway
# tree with one .rs file and asserts the guard's exit status. Guard output
# stays captured, so expected violations do not annotate the CI run.
set -euo pipefail

guard="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/audit-imports.sh"
root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT

cases=0
failures=0

# check <expected-exit> <core|embed> <relative file> <file content>
check() {
  local expected="$1" mode="$2" file="$3" content="$4"
  cases=$((cases + 1))
  local tree="$root/$cases"
  mkdir -p "$tree/$(dirname "$file")"
  printf '%s\n' "$content" >"$tree/$file"

  local status=0 output
  output="$(bash "$guard" "$mode" "$tree" 2>&1)" || status=$?

  local problem=""
  if [ "$status" -ne "$expected" ]; then
    problem="exit $status, expected $expected"
  elif [ "$expected" -eq 1 ] && ! grep -qF "$file" <<<"$output"; then
    problem="violation does not name $file"
  elif [ "$expected" -eq 0 ] && ! grep -q '^OK: ' <<<"$output"; then
    problem="no OK line"
  fi
  if [ -n "$problem" ]; then
    failures=$((failures + 1))
    printf 'FAIL: %s %s %q: %s\n%s\n' "$mode" "$file" "$content" "$problem" "$output"
  fi
}

tab=$'\t'

# Ordinary forbidden imports, each scope and each forbidden crate.
for path in crates/shared crates/domain crates/events crates/core crates/storage crates/auth apps/server/src; do
  check 1 core "$path/lib.rs" 'use daruma_desktop::SomeType;'
done
for crate in daruma_desktop daruma_cli daruma_mcp_bin daruma_web; do
  check 1 core crates/core/src/lib.rs "use $crate::SomeType;"
done
for crate in daruma_storage daruma_events; do
  check 1 embed apps/desktop/src/main.rs "use $crate::Db;"
done

# Whitespace and visibility around the `use`.
check 1 core crates/core/src/lib.rs "use${tab}daruma_cli::SomeType;"
check 1 core crates/core/src/lib.rs 'use   daruma_web::SomeType;'
check 1 core crates/core/src/lib.rs '    pub use daruma_mcp_bin::SomeType;'
check 1 embed apps/desktop/src/main.rs "use${tab}daruma_storage::Db;"
check 1 embed apps/desktop/src/main.rs 'use   daruma_events::EventBus;'
check 1 embed apps/desktop/src/main.rs '    pub(crate) use daruma_storage::Db;'

# Explicitly allowed imports stay allowed.
check 0 embed apps/desktop/src/context.rs 'use daruma_core::embed::{CommandBus, Db};'
check 0 embed apps/desktop/src/context.rs "use${tab}daruma_core::embed::Db;"
check 0 embed apps/desktop/src/context.rs 'use daruma_domain::Task;'
check 0 core crates/core/src/lib.rs 'use daruma_storage::Db;'
check 0 core apps/server/src/main.rs 'use daruma_core::embed::Db;'

# Lookalikes and everything outside a guard's scope.
check 0 core crates/core/src/lib.rs 'use daruma_web_extras::SomeType;'
check 0 embed apps/desktop/src/main.rs 'use daruma_storage_extras::Db;'
check 0 core crates/core/README.md 'use daruma_cli::SomeType;'
check 0 core apps/cli/src/main.rs 'use daruma_desktop::SomeType;'
check 0 embed apps/cli/src/main.rs 'use daruma_storage::Db;'

# A grep that fails to run must not read as a clean tree.
stub="$root/stub-bin"
mkdir -p "$stub"
printf '#!/bin/sh\nexit 2\n' >"$stub/grep"
chmod +x "$stub/grep"
for mode_and_file in core:crates/core/lib.rs embed:apps/desktop/src/main.rs; do
  mode="${mode_and_file%%:*}"
  file="${mode_and_file#*:}"
  cases=$((cases + 1))
  tree="$root/$cases"
  mkdir -p "$tree/$(dirname "$file")"
  printf 'fn main() {}\n' >"$tree/$file"
  status=0
  output="$(PATH="$stub:$PATH" bash "$guard" "$mode" "$tree" 2>&1)" || status=$?
  if [ "$status" -ne 2 ] || grep -qF 'OK: ' <<<"$output"; then
    failures=$((failures + 1))
    printf 'FAIL: %s with broken grep: exit %s\n%s\n' "$mode" "$status" "$output"
  fi
done

if [ "$failures" -gt 0 ]; then
  echo "audit-imports regression: $failures of $cases cases failed."
  exit 1
fi
echo "OK: audit-imports regression, $cases cases."
