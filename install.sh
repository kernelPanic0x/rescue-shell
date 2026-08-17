#!/bin/sh
set -eu
set -o pipefail 2>/dev/null || true

# --- Helpers (defined first) ---
log() { printf '[*] %s\n' "$*" >&2; }
die() { printf '[!] %s\n' "$*" >&2; exit 1; }

# --- Download Helper ---
download_to_stdout() {
    target_url="$1"
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' -fSL --retry 3 "$target_url"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$target_url"
    elif command -v fetch >/dev/null 2>&1; then
        fetch -qo - "$target_url"
    else
        die "curl, wget, or fetch is required"
    fi
}

# --- System Detection ---
is_android() {
    [ -f /system/bin/app_process ] || \
    [ -f /system/build.prop ] || \
    [ -n "${ANDROID_ROOT:-}" ] || \
    command -v getprop >/dev/null 2>&1 || \
    { uname -o 2>/dev/null | grep -qi "android"; }
}

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        if is_android; then
            case "$ARCH" in
                aarch64|arm64) TUPLE="aarch64-linux-android" ;;
                *)             die "unsupported Android architecture: $ARCH" ;;
            esac
        else
            case "$ARCH" in
                x86_64|amd64)         TUPLE="x86_64-unknown-linux-musl" ;;
                aarch64|arm64)        TUPLE="aarch64-unknown-linux-musl" ;;
                armv6*|armv7*|armv8*) TUPLE="arm-unknown-linux-musleabihf" ;;
                *)                    die "unsupported Linux architecture: $ARCH" ;;
            esac
        fi
        ;;
    Darwin)
        # Check if running under Rosetta 2 on Apple Silicon
        if [ "$ARCH" = "x86_64" ] && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null)" = "1" ]; then
            ARCH="arm64"
        fi
        case "$ARCH" in
            x86_64) TUPLE="x86_64-apple-darwin" ;;
            arm64)  TUPLE="aarch64-apple-darwin" ;;
            *)      die "unsupported macOS architecture: $ARCH" ;;
        esac
        ;;
    FreeBSD)
        case "$ARCH" in
            amd64|x86_64)  TUPLE="x86_64-unknown-freebsd" ;;
            arm64|aarch64) TUPLE="aarch64-unknown-freebsd" ;;
            *)             die "unsupported FreeBSD architecture: $ARCH" ;;
        esac
        ;;
    *)
        die "unsupported operating system: $OS"
        ;;
esac

log "target: ${TUPLE}"

# Try to find a writable directory where execution is permitted (not mounted noexec)
can_execute_in() {
    target_dir="${1%/}"
    
    # 1. Basic sanity: must be non-empty, a directory, and writable
    [ -n "$target_dir" ] || return 1
    [ -d "$target_dir" ] || return 1
    [ -w "$target_dir" ] || return 1

    # 2. noexec probe: create a temporary test script and try to execute it
    probe_file="${target_dir}/.exec_test_$$"
    
    printf '#!/bin/sh\nexit 0\n' > "$probe_file" 2>/dev/null || return 1
    chmod 700 "$probe_file" 2>/dev/null || { rm -f "$probe_file"; return 1; }

    if "$probe_file" >/dev/null 2>&1; then
        rm -f "$probe_file"
        return 0
    else
        rm -f "$probe_file"
        return 1
    fi
}

find_temp_dir() {
    for candidate in \
        "${TMPDIR:-}" \
        "${HOME:+$HOME/.local/tmp}" \
        "${HOME:+$HOME/.cache}" \
        "/tmp" \
        "/var/tmp" \
        "/dev/shm" \
        "/data/local/tmp" \
        "${HOME:-}"; do

        [ -n "$candidate" ] && [ ! -d "$candidate" ] && mkdir -p "$candidate" 2>/dev/null || true

        if can_execute_in "$candidate"; then
            printf '%s' "${candidate%/}"
            return 0
        fi
    done

    return 1
}

TARGET_DIR="$(find_temp_dir)" || die "no writable temp dir found"
log "target dir: ${TARGET_DIR}"
BIN="$(mktemp "${TARGET_DIR}/rescue-shell.XXXXXX")" || die "failed to create temporary file"
trap 'rm -f "$BIN"' EXIT INT TERM HUP

URL="https://github.com/kernelPanic0x/rescue-shell/releases/download/latest/rescue-shell-${TUPLE}.gz"

log "downloading ${URL}"
download_to_stdout "$URL" | gzip -dc > "$BIN" || die "download failed (no build for ${TUPLE}?)"
chmod 700 "$BIN"

export WORMHOLE_RELAY_URL="${WORMHOLE_RELAY_URL:-tcp://nbg.ell.dns64.de:4001}"

# Default argument fallback
if [ "$#" -eq 0 ]; then
    set -- serve
fi

# Ensure we have access to a terminal for interactive TUI use
if [ -t 1 ] && [ -r /dev/tty ]; then
    "$BIN" "$@" </dev/tty
elif [ -t 0 ] && [ -t 1 ]; then
    "$BIN" "$@"
else
    die "Interactive TUI requires a terminal (TTY). Running in headless/CI environments is not supported."
fi
