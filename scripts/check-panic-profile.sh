#!/usr/bin/env bash
# Verifies that Cargo.toml sets panic = "unwind" on both release and dev profiles.
# Required so src/connect/runtime.rs TaskGroup can report panics via
# JoinError::into_panic; panic = "abort" terminates the process before
# tokio can classify the join outcome.
#
# Defense-in-depth: runtime.rs also has a compile-time assertion
# (const _: () = assert!(cfg!(panic = "unwind"))). This script protects
# against a profile override slipping in from a config.toml or env var
# at the Cargo.toml level.

set -eu

CARGO_TOML="${1:-Cargo.toml}"

if [ ! -f "${CARGO_TOML}" ]; then
  echo "ERROR: ${CARGO_TOML} not found"
  exit 1
fi

check_profile() {
  local profile="$1"
  local block
  # Extract the [profile.<name>] block (stops at next top-level [section])
  block=$(awk -v prof="[profile.${profile}]" '
    $0 == prof { found = 1; next }
    found && /^\[[^.]/ { found = 0 }
    found { print }
  ' "${CARGO_TOML}")

  if [ -z "${block}" ]; then
    echo "ERROR: [profile.${profile}] section missing from ${CARGO_TOML}"
    return 1
  fi

  local panic_line
  panic_line=$(echo "${block}" | grep -E '^\s*panic\s*=' || true)

  if [ -z "${panic_line}" ]; then
    echo "ERROR: panic setting missing in [profile.${profile}]"
    return 1
  fi

  if ! echo "${panic_line}" | grep -qE '^\s*panic\s*=\s*"unwind"\s*$'; then
    echo "ERROR: [profile.${profile}] has: ${panic_line}"
    echo "       expected: panic = \"unwind\""
    return 1
  fi

  echo "  [profile.${profile}]: panic = \"unwind\" OK"
}

status=0
check_profile "dev" || status=1
check_profile "release" || status=1

if [ "${status}" -ne 0 ]; then
  echo ""
  echo "panic profile check FAILED."
  echo "TaskGroup shutdown reporting requires panic = \"unwind\" on both profiles."
  exit 1
fi

echo "panic profile: OK"
