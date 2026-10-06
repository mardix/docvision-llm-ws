#!/bin/sh
# Service overhead per RPC (budget: p99 <= 2 ms).
set -eu
. "$(dirname "$0")/common.sh"
oha -z "$DURATION" -c "$CONCURRENCY" -m POST \
  -H "X-ACCESS-TOKEN: $DOCVISION_TOKEN" -H "Content-Type: application/json" \
  -d '{"operation":"health"}' "$DOCVISION_URL/rpc"
