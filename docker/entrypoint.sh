#!/bin/sh
# Prepares the SSH key pair on first start, then runs elkarbackup.
set -eu

if [ "${1:-}" = "serve" ]; then
    elkarbackup generate-keypair --if-missing >/dev/null
fi

exec elkarbackup "$@"
