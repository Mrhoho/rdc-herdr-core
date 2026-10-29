#!/bin/sh
# Freeze the default-build hc before feature builds overwrite target/debug.
# Both processes in the cross-distribution test use private scratch roots.
set -eu
cd "$(dirname "$0")/.."
scratch=$(mktemp -d /var/tmp/nc-a2.XXXXXX)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
cargo build --locked -p herdr-core --bin herdr-core
cp "${CARGO_TARGET_DIR:-target}/debug/herdr-core" "$scratch/hc"
cargo clippy --locked -p herdr-core --features managed-test --all-targets -- -D warnings
cargo test --locked -p herdr-core --features managed-test --lib
NEARSHELL_HC_UPSTREAM_TEST_BIN="$scratch/hc" cargo test --locked -p herdr-core --features managed-test --test managed -- --include-ignored
