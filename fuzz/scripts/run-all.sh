#!/usr/bin/env bash
# Runs every fuzz target for a fixed time, one after another, with the limits documented
# in docs/security/fuzzing.md. Logs go to fuzz/logs/<target>.log.
#
#   fuzz/scripts/run-all.sh [seconds per target] [target...]
#
# Exits non-zero when any target found a crash, timeout or out-of-memory input; the input
# is then under fuzz/artifacts/<target>/.
set -uo pipefail

cd "$(dirname "$0")/.."
SECONDS_PER_TARGET="${1:-60}"
shift || true
TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  while IFS= read -r t; do TARGETS+=("$t"); done < <(cargo +nightly fuzz list)
fi

# A small quarantine and short allocation stacks keep AddressSanitizer's own memory from
# growing into the RSS limit on targets with deep, varied recursion (route_pattern).
export ASAN_OPTIONS="${ASAN_OPTIONS:-quarantine_size_mb=64:malloc_context_size=4}"

python3 -I scripts/seed-corpora.py >/dev/null
mkdir -p logs

max_len() {
  case "$1" in
    openapi_document|documents_json|mcp_tools) echo 65536 ;;
    inbound_stream|websocket_response) echo 16384 ;;
    *) echo 4096 ;;
  esac
}

run() {
  local target="$1"
  cargo +nightly fuzz run "$target" -- \
    -max_total_time="$SECONDS_PER_TARGET" \
    -max_len="$(max_len "$target")" \
    -timeout=10 \
    -rss_limit_mb=2048 \
    -print_final_stats=1 \
    >"logs/$target.log" 2>&1
  local status=$?
  if [ $status -ne 0 ]; then
    echo "FAIL $target (exit $status, see fuzz/logs/$target.log)"
  else
    echo "ok   $target"
  fi
  return $status
}

failed=0
for target in "${TARGETS[@]}"; do
  run "$target" || failed=1
done
exit $failed
