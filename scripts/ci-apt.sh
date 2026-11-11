#!/usr/bin/env bash
# Installs packages on a CI runner. First from the runner's own mirror and
# package lists; if that fails (the Azure mirror runners use has stopped
# answering for hours at a time), from Ubuntu's main archive instead, with
# fresh lists and a time limit on each try.
#   scripts/ci-apt.sh package...
set -uo pipefail
apt() { sudo env DEBIAN_FRONTEND=noninteractive timeout 300 apt-get -o Acquire::Retries=2 -o Acquire::http::Timeout=20 "$@"; }
apt install -y "$@" && exit 0
echo "install failed: switching from the runner's mirror to archive.ubuntu.com" >&2
for f in /etc/apt/apt-mirrors.txt /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list; do
    [ -f "$f" ] && sudo sed -i 's#http://azure\.archive\.ubuntu\.com/ubuntu#http://archive.ubuntu.com/ubuntu#g' "$f"
done
for try in 1 2 3; do
    apt update && apt install -y "$@" && exit 0
    echo "apt try $try failed; trying again" >&2
    sleep 10
done
exit 1
