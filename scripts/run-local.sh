#!/usr/bin/env bash
# Build and run docvision-llm-ws locally.
#
#   scripts/run-local.sh            # release build, uses .env (created on first run)
#   scripts/run-local.sh --mock     # also start the mock LLM provider and point the service at it
#   scripts/run-local.sh --debug    # debug build (faster to compile), text logs
#
# Build output goes to $CARGO_TARGET_DIR (default: ~/.cache/docvision-llm-ws/target) so it stays
# out of synced folders. Stop with Ctrl-C (the mock provider is stopped too).
set -euo pipefail
cd "$(dirname "$0")/.."

profile=release
mock=false
for arg in "$@"; do
  case "$arg" in
    --mock) mock=true ;;
    --debug) profile=debug ;;
    -h | --help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

command -v cargo >/dev/null || { echo "cargo not found: install Rust from https://rustup.rs" >&2; exit 1; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/docvision-llm-ws/target}"

[ -f .env ] || ./scripts/setup-env.sh

build_flag=""
[ "$profile" = release ] && build_flag="--release"
echo "building ($profile)..."
cargo build $build_flag --bin docvision-llm-ws
bin="$CARGO_TARGET_DIR/$profile/docvision-llm-ws"

# Values from .env (real environment variables still win inside the service).
env_get() { grep -E "^$1=" .env | tail -1 | cut -d= -f2-; }
token="${DOCVISION_TOKEN:-$(env_get DOCVISION_TOKEN)}"
bind="${DOCVISION_BIND:-$(env_get DOCVISION_BIND)}"
bind="${bind:-0.0.0.0:4242}"
port="${bind##*:}"

mock_pid=""
svc_pid=""
cleanup() { for p in $svc_pid $mock_pid; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM

if $mock; then
  cargo build $build_flag --example mock_provider
  MOCK_ADDR=127.0.0.1:9099 MOCK_LATENCY_MS="${MOCK_LATENCY_MS:-200-800}" "$CARGO_TARGET_DIR/$profile/examples/mock_provider" &
  mock_pid=$!
  export DOCVISION_LLM_PROVIDER=openai
  export DOCVISION_LLM_BASE_URL=http://127.0.0.1:9099/v1
  export DOCVISION_LLM_MODEL=mock-model
  export DOCVISION_LLM_API_KEY=mock
  echo "mock LLM provider on http://127.0.0.1:9099 (stats: /stats)"
fi

[ "$profile" = debug ] && export DOCVISION_LOG_FORMAT="${DOCVISION_LOG_FORMAT:-text}"

cat <<EOF

docvision-llm-ws on http://127.0.0.1:$port  (data: $(env_get DOCVISION_DATA_DIR || true))
Try:
  curl -s localhost:$port/rpc -H "X-ACCESS-TOKEN: $token" -H 'Content-Type: application/json' \\
    -d '{"operation":"convert","payload":{"source":"$PWD/README.md"},"options":{"execution":"sync","summary_method":"local","title_method":"local"}}'

EOF

"$bin" &
svc_pid=$!
wait "$svc_pid"
