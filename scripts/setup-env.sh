#!/bin/sh
# Create .env from .env.example with a freshly generated access token (first local setup).
set -eu
cd "$(dirname "$0")/.."
if [ -f .env ]; then
  echo ".env already exists; leaving it unchanged" >&2
  exit 0
fi
token=$(head -c 32 /dev/urandom | base64 | tr -d '/+=\n' | cut -c1-40)
sed "s|^DOCVISION_TOKEN=.*|DOCVISION_TOKEN=${token}|" .env.example > .env
chmod 600 .env
echo "wrote .env (token generated)"
