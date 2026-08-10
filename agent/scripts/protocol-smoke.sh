#!/usr/bin/env bash
# Protocol smoke test — exercises the built binary end to end.
# Usage: scripts/protocol-smoke.sh [path-to-binary]
set -uo pipefail

BIN="${1:-bin/puppetterm-agent}"
if [ ! -x "$BIN" ]; then
  echo "binary not found: $BIN (run 'make build' first)" >&2
  exit 1
fi

fail=0
check() { # check <desc> <condition-output-capture> ...
  local desc="$1"; shift
  if "$@"; then echo "  ok: $desc"; else echo "  FAIL: $desc"; fail=1; fi
}

echo "== 1. run echo"
out=$(printf '%s' '{"action":"run","params":{"cmd":"echo hi"}}' | "$BIN")
# note: newlines are JSON-escaped in the raw stream, so match `"hi` not `"hi"`
check "streams stdout" grep -q '"hi' <<<"$out"
check "result exit 0" grep -q '"exit":0' <<<"$out"

echo "== 2. run failing command"
out=$(printf '%s' '{"action":"run","params":{"cmd":"exit 3"}}' | "$BIN")
check "result exit 3" grep -q '"exit":3' <<<"$out"

echo "== 3. stderr captured"
out=$(printf '%s' '{"action":"run","params":{"cmd":"echo err 1>&2"}}' | "$BIN")
check "stderr event present" grep -q '"stream":"stderr"' <<<"$out"

echo "== 4. snapshot structured"
out=$(printf '%s' '{"action":"snapshot"}' | "$BIN")
check "structured result" grep -q '"structured"' <<<"$out"
check "hostname present" grep -q '"hostname"' <<<"$out"

echo "== 5. unknown action -> error + exit 1"
set +e
out=$(printf '%s' '{"action":"nope"}' | "$BIN"); code=$?
set -e
check "exit code 1" [ "$code" -eq 1 ]
check "error event" grep -q '"type":"error"' <<<"$out"

echo "== 6. malformed json -> error + exit 1"
set +e
out=$(printf '%s' '{not json' | "$BIN"); code=$?
set -e
check "exit code 1" [ "$code" -eq 1 ]
check "error event" grep -q '"type":"error"' <<<"$out"

echo "== 7. timeout"
set +e
start=$(date +%s)
out=$(printf '%s' '{"action":"run","params":{"cmd":"sleep 30"},"timeout_ms":500}' | "$BIN"); code=$?
elapsed=$(( $(date +%s) - start ))
set -e
check "exit code 124" [ "$code" -eq 124 ]
check "fast (<=5s, took ${elapsed}s)" [ "$elapsed" -le 5 ]
check "error event" grep -q '"type":"error"' <<<"$out"

echo "== 8. empty request -> error + exit 1"
set +e
out=$(printf '' | "$BIN"); code=$?
set -e
check "exit code 1" [ "$code" -eq 1 ]

if [ "$fail" -eq 0 ]; then
  echo "ALL PASS"
else
  echo "SOME FAILED" >&2
  exit 1
fi
