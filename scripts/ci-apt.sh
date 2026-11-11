#!/usr/bin/env bash
# Installs packages on a CI runner: apt can hang on a slow mirror, so each
# try gets five minutes, three tries in all.
#   scripts/ci-apt.sh package...
set -uo pipefail
export DEBIAN_FRONTEND=noninteractive
for try in 1 2 3; do
    if sudo -E timeout 300 apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 update -qq &&
        sudo -E timeout 300 apt-get -o Acquire::Retries=3 install -y -qq "$@"; then
        exit 0
    fi
    echo "apt try $try failed or timed out; trying again" >&2
    sleep 10
done
exit 1
