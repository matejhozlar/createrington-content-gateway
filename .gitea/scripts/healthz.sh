#!/usr/bin/env bash
set -euo pipefail

image=curlimages/curl:8.16.0

if [ "$1" = --pull ]; then
  docker pull --quiet "$image"
  exit
fi

dir=$1

value() {
  grep -E "^(export )?$1=" "$dir/.env" | tail -n 1 | cut -d= -f2- \
    | sed -E "s/^[[:space:]]*//; s/[[:space:]]*$//; s/^'(.*)'$/\1/; s/^\"(.*)\"$/\1/"
}

host=$(value CONTENT_GATEWAY_HOST || true)
port=$(value CONTENT_GATEWAY_PORT || true)
case "$host" in
  "" | 0.0.0.0) host=127.0.0.1 ;;
  ::) host="[::1]" ;;
  *:*) host="[$host]" ;;
esac

docker run --rm --pull never --network host "$image" \
  -fsS --max-time 5 "http://${host}:${port:-5005}/v1/healthz"
