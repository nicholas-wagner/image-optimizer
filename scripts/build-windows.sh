#!/bin/sh
# Build the single Windows exe. Needs Docker.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
exec docker run --rm -v "$root":/src -w /src rust:1-bookworm bash scripts/build-windows-inner.sh
