#!/usr/bin/env bash
# Full test suite: unit tests + integration tests against an ephemeral
# Postgres cluster (created, used, torn down; no system services touched).
set -euo pipefail
cd "$(dirname "$0")/.."

datadir="${TMPDIR:-/tmp}/dpm-test-pg-$$"
cleanup() { scripts/ephemeral-pg.sh stop "$datadir" || true; rm -rf "$datadir"; }
trap cleanup EXIT

url="$(scripts/ephemeral-pg.sh start "$datadir" 54329)"
echo "ephemeral postgres: $url"

DPM_TEST_DATABASE_URL="$url" cargo test "$@" -- --test-threads=4
