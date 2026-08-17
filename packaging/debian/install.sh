#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run this installer as root" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/../.." && pwd)
binary="$project_dir/target/release/bootwitness"
unit="$project_dir/packaging/systemd/bootwitness.service"

if [ ! -x "$binary" ]; then
    echo "error: $binary is missing; run 'cargo build --locked --release' first" >&2
    exit 1
fi

install -D -m 0755 "$binary" /usr/bin/bootwitness
install -D -m 0644 "$unit" /usr/lib/systemd/system/bootwitness.service
install -D -m 0644 "$project_dir/README.md" /usr/share/doc/bootwitness/README.md
install -D -m 0644 "$project_dir/LICENSE" /usr/share/doc/bootwitness/LICENSE
systemctl daemon-reload
systemctl enable --now bootwitness.service

echo "bootwitness installed and started"
systemctl --no-pager --full status bootwitness.service
