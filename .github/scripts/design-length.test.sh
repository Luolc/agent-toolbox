#!/bin/sh
# Checks the three exits of design-length.sh: 0 at the limit, 1 above it,
# 4 for a file that is missing.
dir="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0

lines() { awk -v n="$1" 'BEGIN { for (i = 0; i < n; i++) print "x" }'; }

expect() {
  want="$1"; shift
  "$dir/design-length.sh" "$@" >/dev/null 2>&1
  got=$?
  if [ "$got" -ne "$want" ]; then
    echo "FAIL: $* exited $got, want $want" >&2
    fail=1
  fi
}

lines 200 > "$tmp/at-limit"
lines 201 > "$tmp/over"
expect 0 "$tmp/at-limit"
expect 1 "$tmp/over"
expect 4 "$tmp/missing"
exit "$fail"
