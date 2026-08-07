#!/usr/bin/env bash
# usage: curl -sL https://raw.githubusercontent.com/kernelPanic0x/rescue-shell/main/install.sh | bash
set -euo pipefail

ARCH="$(uname -m)"

case "$ARCH" in
    x86_64)               TUPLE="x86_64-unknown-linux-musl" ;;
    aarch64)              TUPLE="aarch64-unknown-linux-musl" ;;
    armv6l|armv7l|armv8l) TUPLE="arm-unknown-linux-musleabihf" ;;
    *)                    die "unsupported architecture: $ARCH" ;;
esac

URL="https://github.com/kernelPanic0x/rescue-shell/releases/download/latest/rescue-shell-${TUPLE}"

log() { printf '[*] %s\n' "$*" >&2; }
die() { printf '[!] %s\n' "$*" >&2; exit 1; }

log "target: ${TUPLE}"

BIN="$(mktemp "${TMPDIR:-/tmp}/rescue-shell.XXXXXX")" || die "no writable temp dir"
trap 'rm -f "$BIN"' EXIT

log "downloading ${URL}"
curl -fSL --retry 3 -o "$BIN" "$URL" || die "download failed (no build for ${TUPLE}?)"
chmod 700 "$BIN"

export WORMHOLE_RELAY_URL="${WORMHOLE_RELAY_URL:-tcp://nbg.ell.dns64.de:4001}"

# If no arguments were passed to the script, default to "serve"
if [ "$#" -eq 0 ]; then
    set -- serve
fi

# </dev/tty only on the child: bash itself must keep reading the script from the pipe
if [ -r /dev/tty ]; then
    "$BIN" "$@" </dev/tty
else
    "$BIN" "$@"
fi
