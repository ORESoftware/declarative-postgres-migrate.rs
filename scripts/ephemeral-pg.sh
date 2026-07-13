#!/usr/bin/env bash
# Boot a throwaway Postgres cluster for dpm development/tests.
# Usage:
#   scripts/ephemeral-pg.sh start [datadir] [port]   # prints the URL
#   scripts/ephemeral-pg.sh stop  [datadir]
# No system services are touched; the cluster lives entirely in datadir.
set -euo pipefail

cmd="${1:-start}"
datadir="${2:-${TMPDIR:-/tmp}/dpm-ephemeral-pg}"
port="${3:-54329}"

case "$cmd" in
  start)
    # LC_ALL=C avoids "postmaster became multithreaded during startup" on
    # macOS; the unix socket lives in the datadir to avoid /tmp collisions.
    export LC_ALL=C LANG=C
    if [ ! -d "$datadir/base" ]; then
      mkdir -p "$datadir"
      initdb -D "$datadir" -U postgres -A trust -E UTF8 --no-locale >/dev/null
    fi
    if ! pg_ctl -D "$datadir" status >/dev/null 2>&1; then
      pg_ctl -D "$datadir" \
        -o "-p $port -c listen_addresses=127.0.0.1 -c unix_socket_directories='$datadir' -c fsync=off -c full_page_writes=off" \
        -l "$datadir/log" -w start >/dev/null
    fi
    echo "postgres://postgres@127.0.0.1:$port/postgres"
    ;;
  stop)
    pg_ctl -D "$datadir" -m immediate stop >/dev/null 2>&1 || true
    ;;
  *)
    echo "usage: $0 start|stop [datadir] [port]" >&2
    exit 1
    ;;
esac
