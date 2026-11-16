#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../packages/python-client"
rm -rf dist
uv build

venv=$(mktemp -d)
trap 'rm -rf "$venv"' EXIT
uv venv --quiet "$venv"
wheels=(dist/*.whl)
uv pip install --quiet --python "$venv" "${wheels[@]}"

python="$venv/bin/python"
[ -x "$python" ] || python="$venv/Scripts/python.exe"
cd "$venv"
"$python" -I -c '
from importlib.metadata import version

from content_gateway import AsyncContentGateway, ContentGateway

print("Imported", ContentGateway.__name__, "and", AsyncContentGateway.__name__, "from content-gateway-client", version("content-gateway-client"))
'
