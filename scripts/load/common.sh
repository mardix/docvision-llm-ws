# Shared settings for load scripts. Override via environment.
: "${DOCVISION_URL:=http://127.0.0.1:4242}"
: "${DOCVISION_TOKEN:?set DOCVISION_TOKEN}"
: "${DURATION:=15s}"
: "${CONCURRENCY:=64}"
