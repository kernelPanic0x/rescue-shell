#!/usr/bin/env bash
# usage: curl -sL https://raw.githubusercontent.com/kernelPanic0x/rescue-shell/main/install.sh | bash
set -euo pipefail

TUPLE="$(uname -m)-unknown-linux-musl"
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

# </dev/tty only on the child: bash itself must keep reading the script from the pipe
if [ -r /dev/tty ]; then
    "$BIN" serve "$@" </dev/tty
else
    "$BIN" serve "$@"
fi
