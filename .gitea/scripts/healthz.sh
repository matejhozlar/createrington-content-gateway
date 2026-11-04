#!/usr/bin/env bash
# Prints what the gateway deployed in the given directory answers on /v1/healthz,
# read through the host's network since a job container's loopback is its own.
# Exits non-zero when it does not answer.
set -euo pipefail

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

docker run --rm --network host curlimages/curl:8.16.0 \
  -fsS --max-time 5 "http://${host}:${port:-5005}/v1/healthz"
