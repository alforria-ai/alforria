#!/usr/bin/env bash
# Run the test suite proving it touches none of the developer's own files.
#
# HOME and every XDG_* directory point at an empty directory that is made
# read-only; the only writable place is a sandbox subtree (TMPDIR). The run
# fails if a test fails *or* if anything appears under the read-only home.
# `alforria_core::paths::sandbox` makes the same mistake a panic at its
# source: a test run refuses global paths outside the temporary directory.
#
# Usage: scripts/test-isolated.sh [extra `cargo nextest run` arguments]
#   ALFORRIA_ISOLATION_ROOT  where to create the run's directory (default: $TMPDIR or /tmp)
set -euo pipefail

cd "$(dirname "$0")/.."

# cargo and rustup must keep finding the real toolchain once HOME moves.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"

base="${ALFORRIA_ISOLATION_ROOT:-${TMPDIR:-/tmp}}"
mkdir -p "$base"
root="$(mktemp -d "$base/alforria-isolation.XXXXXX")"
home="$root/home"
sandbox="$root/sandbox"
mkdir -p "$home" "$sandbox"
chmod 555 "$home"
cleanup() { chmod -R u+w "$root" 2>/dev/null || true; rm -rf "$root"; }
trap cleanup EXIT

# No provider key, token or server credential reaches a test.
for name in $(env | cut -d= -f1); do
  case "$name" in
    *_API_KEY|*_TOKEN|*_SECRET*|AWS_*|OPENCODE_*|ALFORRIA_*|LIBERTAI_*) unset "$name" ;;
  esac
done

status=0
env \
  HOME="$home" \
  OPENCODE_TEST_HOME="$home" \
  XDG_CONFIG_HOME="$home/config" \
  XDG_DATA_HOME="$home/data" \
  XDG_STATE_HOME="$home/state" \
  XDG_CACHE_HOME="$home/cache" \
  TMPDIR="$sandbox" \
  ALFORRIA_TEST_GUARD=1 \
  LIBERTAI_MODEL_CATALOG_URL="" \
  cargo nextest run --workspace "$@" || status=$?

leaked="$(find "$home" -mindepth 1 2>/dev/null || true)"
if [ -n "$leaked" ]; then
  echo "isolation: FAILED — the test run created files under its read-only home:" >&2
  echo "$leaked" >&2
  exit 1
fi
if [ "$status" -ne 0 ]; then
  echo "isolation: tests failed (exit $status); nothing was written under the read-only home" >&2
  exit "$status"
fi
echo "isolation: OK — nothing was written outside the sandbox ($sandbox)"
