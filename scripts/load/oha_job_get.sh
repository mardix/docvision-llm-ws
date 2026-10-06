#!/bin/sh
# Read-only RPC throughput (budget: >= 20,000 req/s on 4 vCPU). Needs JOB_ID of a finished job.
set -eu
. "$(dirname "$0")/common.sh"
: "${JOB_ID:?set JOB_ID (a completed job)}"
oha -z "$DURATION" -c "$CONCURRENCY" -m POST \
  -H "X-ACCESS-TOKEN: $DOCVISION_TOKEN" -H "Content-Type: application/json" \
  -d "{\"operation\":\"job.get\",\"payload\":{\"job_id\":\"$JOB_ID\",\"fields\":[\"status\"]}}" "$DOCVISION_URL/rpc"
