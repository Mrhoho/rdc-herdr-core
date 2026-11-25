#!/bin/sh
# Linux: the remote build's lint, lib tests and contract suites (nearshell-test).
# Freeze a default-build hc first, as check-managed.sh does: the
# cross-distribution test runs it in its own scratch session beside nhc.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Linux ] || { echo "check-remote: the remote build is Linux only" >&2; exit 2; }
scratch=$(mktemp -d /var/tmp/nc-remote.XXXXXX)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
cargo build --locked -p herdr-core --bin herdr-core
cp "${CARGO_TARGET_DIR:-target}/debug/herdr-core" "$scratch/hc"
# Builds of differing identity for tests/remote_machines.rs (test builds take
# it unchecked; only herdr-core rebuilds for each): cur is the hub's build,
# the others differ from it in release, commit or bundle ID.
variant() {
  NEARSHELL_CORE_RELEASE_SEQ=$2 NEARSHELL_CORE_COMMIT=$(printf "%040d" "$3") NEARSHELL_CORE_BUNDLE_ID=$4 \
    NEARSHELL_CORE_PATCHSET=check-remote NEARSHELL_CORE_CONFIG=dev \
    cargo build --locked -p herdr-core --features nearshell-test --bin nhc
  cp "${CARGO_TARGET_DIR:-target}/debug/nhc" "$scratch/nhc-$1"
}
variant cur 2 1 com.nearshell.remote-test
variant old 1 0 com.nearshell.remote-test
variant fork 2 9 com.nearshell.remote-test
variant other 2 1 com.nearshell.remote-test-other
cargo clippy --locked -p herdr-core --features nearshell-test --all-targets -- -D warnings
cargo test --locked -p herdr-core --features nearshell-test --lib
NEARSHELL_HC_UPSTREAM_TEST_BIN="$scratch/hc" NEARSHELL_REMOTE_TEST_BINS="$scratch" \
  cargo test --locked -p herdr-core --features nearshell-test --test remote_build --test remote_machines -- --include-ignored
