#!/usr/bin/env bash
# Installs packages on a CI runner. The runner's package lists usually do,
# so no `apt-get update` first (it has hung for most of an hour on runners);
# only if the install fails, update and try again, with a time limit.
#   scripts/ci-apt.sh package...
set -uo pipefail
apt() { sudo env DEBIAN_FRONTEND=noninteractive timeout 300 apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 "$@"; }
apt install -y "$@" && exit 0
for try in 1 2 3; do
    echo "install failed; updating the package lists (try $try)" >&2
    apt update && apt install -y "$@" && exit 0
    sleep 10
done
exit 1
