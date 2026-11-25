#!/usr/bin/env bash
# Interop test: the pure-Rust ENet client (crates/hestia-stream/src/enet.rs)
# against the ENet fork Sunshine/Hermes/moonlight-common-c use (cgutman/enet).
# Tests clean IPv4 and IPv6 links, then an IPv4 UDP proxy that drops 1/3 of
# the datagrams in each direction, with fragmented reliable messages.
#
# Usage: tools/enet-interop/run.sh [path-to-cgutman-enet-checkout]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
cleanup() {
    [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

enet="${1:-}"
if [[ -z "$enet" ]]; then
    enet="$work/enet"
    git clone --depth 1 -q https://github.com/cgutman/enet.git "$enet"
fi

gcc -O1 -DHAS_SOCKLEN_T -DHAS_POLL -DHAS_FCNTL -DHAS_GETADDRINFO -DHAS_GETNAMEINFO \
    -DHAS_INET_PTON -DHAS_INET_NTOP -DHAS_MSGHDR_FLAGS -I"$enet/include" "$here/server.c" \
    "$enet"/callbacks.c "$enet"/compress.c "$enet"/host.c "$enet"/list.c "$enet"/packet.c \
    "$enet"/peer.c "$enet"/protocol.c "$enet"/unix.c -o "$work/enet_server"
rustc --edition 2024 -O "$here/client.rs" -o "$work/enet_client"
rustc --edition 2024 -O "$here/lossy.rs" -o "$work/enet_lossy"

run_case() {
    local port="$1" client="$2" limit="$3" loopback="${4:-127.0.0.1}"
    "$work/enet_server" "$port" "$loopback" >"$work/server-$port.log" &
    server_pid=$!
    sleep 0.3
    timeout "$limit" "$client" "$port" "$loopback"
    sleep 1
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=
    if ! grep -q DISCONNECT "$work/server-$port.log"; then
        echo "server did not see a clean disconnect:" >&2
        cat "$work/server-$port.log" >&2
        exit 1
    fi
}

run_case 40999 "$work/enet_client" 30
run_case 40998 "$work/enet_client" 30 ::1
run_case 40997 "$work/enet_lossy" 60
echo "ENet interop OK"
