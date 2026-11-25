#!/bin/sh
# Linux: the remote build's lint, lib tests and contract suite (nearshell-test).
# Freeze a default-build hc first, as check-managed.sh does: the
# cross-distribution test runs it in its own scratch session beside nhc.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Linux ] || { echo "check-remote: the remote build is Linux only" >&2; exit 2; }
scratch=$(mktemp -d /var/tmp/nc-remote.XXXXXX)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
cargo build --locked -p herdr-core --bin herdr-core
cp "${CARGO_TARGET_DIR:-target}/debug/herdr-core" "$scratch/hc"
cargo clippy --locked -p herdr-core --features nearshell-test --all-targets -- -D warnings
cargo test --locked -p herdr-core --features nearshell-test --lib
NEARSHELL_HC_UPSTREAM_TEST_BIN="$scratch/hc" cargo test --locked -p herdr-core --features nearshell-test --test remote_build -- --include-ignored
