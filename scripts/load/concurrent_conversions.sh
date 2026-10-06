#!/bin/sh
# Submit N async conversions (default 500) against the mock provider and wait for them all.
# Start the mock first: MOCK_LATENCY_MS=2000-8000 cargo run --release --example mock_provider
# and the service with DOCVISION_LLM_BASE_URL=http://127.0.0.1:9099/v1 DOCVISION_LLM_MODEL=mock-model.
set -eu
. "$(dirname "$0")/common.sh"
: "${N:=500}"
: "${SOURCE:?set SOURCE to an absolute path of a PDF}"
ids=""
for i in $(seq 1 "$N"); do
  id=$(curl -s -H "X-ACCESS-TOKEN: $DOCVISION_TOKEN" -H 'Content-Type: application/json' \
    -d "{\"operation\":\"convert\",\"payload\":{\"source\":\"$SOURCE\"},\"options\":{\"cache\":\"bypass\",\"gen_summary\":false,\"gen_title\":false}}" \
    "$DOCVISION_URL/rpc" | sed -n 's/.*"job_id":"\([^"]*\)".*/\1/p')
  ids="$ids $id"
done
echo "submitted $N"
done_n=0
for id in $ids; do
  while :; do
    st=$(curl -s -H "X-ACCESS-TOKEN: $DOCVISION_TOKEN" -H 'Content-Type: application/json' \
      -d "{\"operation\":\"job.wait\",\"payload\":{\"job_id\":\"$id\",\"timeout_ms\":30000}}" "$DOCVISION_URL/rpc" | sed -n 's/.*"status":"\([a-z]*\)".*/\1/p' | head -1)
    case "$st" in completed|partial|failed) break;; esac
  done
  [ "$st" = completed ] && done_n=$((done_n+1))
done
echo "completed $done_n / $N"
