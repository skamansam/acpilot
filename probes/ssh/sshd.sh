#!/usr/bin/env bash
# P0-4's harness, without a container runtime.
#
# TODO.md asks for `docker compose up sshd`. Where a container is not available —
# and this checkout's host is WSL2 with no docker integration — the distro's own
# sshd does the same job: a real SSH server, a real key exchange, a real channel.
# Nothing here is a mock, and nothing is privileged: sshd listens on loopback on a
# high port, authenticates only the current user with a key generated below, and
# turns off password auth, PAM, and every subsystem it does not need.
#
#   ./sshd.sh start     start it (idempotent) and print the --key to use
#   ./sshd.sh stop      stop it
#   ./sshd.sh status    is it running?
#
# The fixture lives under target/, which is gitignored, so no key is ever
# committed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixture="$here/target/sshd-fixture"
port="${ACPILOT_SSH_PORT:-2222}"
user="${USER:-$(id -un)}"
ssh_dir="${SSHD_DIR:-/usr/sbin}"
sshd="$ssh_dir/sshd"

start() {
    if [ ! -x "$sshd" ]; then
        echo "no sshd at $sshd; set SSHD_DIR (macOS: /usr/sbin, Debian: /usr/sbin)" >&2
        exit 1
    fi
    mkdir -p "$fixture"
    if [ ! -f "$fixture/hostkey" ]; then
        ssh-keygen -t ed25519 -f "$fixture/hostkey" -N '' -q -C acpilot-p0-4-host
    fi
    if [ ! -f "$fixture/clientkey" ]; then
        ssh-keygen -t ed25519 -f "$fixture/clientkey" -N '' -q -C acpilot-p0-4-client
    fi
    cp "$fixture/clientkey.pub" "$fixture/authorized_keys"
    chmod 600 "$fixture/authorized_keys"

    cat > "$fixture/sshd_config" <<EOF
Port $port
ListenAddress 127.0.0.1
HostKey $fixture/hostkey
AuthorizedKeysFile $fixture/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
PidFile $fixture/sshd.pid
LogLevel VERBOSE
EOF

    if [ -f "$fixture/sshd.pid" ] && kill -0 "$(cat "$fixture/sshd.pid")" 2>/dev/null; then
        echo "already running on 127.0.0.1:$port (pid $(cat "$fixture/sshd.pid"))"
    else
        "$sshd" -f "$fixture/sshd_config" -E "$fixture/sshd.log"
        sleep 0.3
        echo "listening on 127.0.0.1:$port as $user (pid $(cat "$fixture/sshd.pid"))"
    fi
    echo "key: $fixture/clientkey"
    echo
    echo "$here/target/release/ssh-probe bench --key $fixture/clientkey --user $user"
}

stop() {
    if [ -f "$fixture/sshd.pid" ] && kill -0 "$(cat "$fixture/sshd.pid")" 2>/dev/null; then
        kill "$(cat "$fixture/sshd.pid")"
        echo "stopped"
    else
        echo "not running"
    fi
}

status() {
    if [ -f "$fixture/sshd.pid" ] && kill -0 "$(cat "$fixture/sshd.pid")" 2>/dev/null; then
        echo "running on 127.0.0.1:$port (pid $(cat "$fixture/sshd.pid"))"
    else
        echo "not running"
        exit 1
    fi
}

case "${1:-start}" in
    start) start ;;
    stop) stop ;;
    status) status ;;
    *) echo "usage: $0 [start|stop|status]" >&2; exit 2 ;;
esac
