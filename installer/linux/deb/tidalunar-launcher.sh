#!/bin/sh
set -eu

USER_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tidalunar"
SYS_DIR="/usr/lib/tidalunar"
# Lock in the per-user runtime dir, falling back to the (user-owned) data dir,
# never world-writable /tmp.
LOCK="${XDG_RUNTIME_DIR:-$USER_DIR}/tidalunar-launcher.lock"

# 1. First-launch payload extraction, race-free via flock.
if [ ! -x "$USER_DIR/tidalunar" ]; then
    mkdir -p "$USER_DIR"
    (
        flock -x 9
        if [ ! -x "$USER_DIR/tidalunar" ]; then
            # Explicit --zstd (don't rely on tar autodetection). On failure,
            # drop the marker binary, letting the next launch re-extract instead
            # of exec'ing a truncated tree; keep cache/auth untouched.
            if ! tar --zstd --no-same-owner --no-overwrite-dir --no-same-permissions \
                    -xf "$SYS_DIR/payload.tar.zst" -C "$USER_DIR"; then
                printf '%s\n' "tidalunar: payload extraction failed" >&2
                rm -f "$USER_DIR/tidalunar"
                exit 1
            fi
        fi
    ) 9>"$LOCK"
fi

# 2. Sandbox helper protocol-version compat check.
USER_PROTO_FILE="$USER_DIR/SANDBOX_PROTOCOL_REQUIRED"
SYS_PROTO_FILE="$SYS_DIR/SANDBOX_PROTOCOL_VERSION"
USER_PROTO=0
SYS_PROTO=0
if [ -f "$USER_PROTO_FILE" ]; then
    USER_PROTO=$(tr -cd '0-9' < "$USER_PROTO_FILE" 2>/dev/null | head -c 10)
fi
if [ -f "$SYS_PROTO_FILE" ]; then
    SYS_PROTO=$(tr -cd '0-9' < "$SYS_PROTO_FILE" 2>/dev/null | head -c 10)
fi
USER_PROTO=${USER_PROTO:-0}
SYS_PROTO=${SYS_PROTO:-0}

if [ "$USER_PROTO" -gt "$SYS_PROTO" ]; then
    MSG="TidaLunar's bundled sandbox helper is older than the in-app version expects.
Run 'sudo apt upgrade tidalunar' (or download a newer .deb from
https://github.com/Brskt/TidaLuna-lunar/releases) and try again."
    if hash notify-send 2>/dev/null && [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        notify-send -i tidalunar "TidaLunar update needed" "$MSG"
    fi
    printf '%s\n' "$MSG" >&2
    exit 1
fi

# 3. Tell CEF to look for chrome-sandbox at the system path, not in user home.
export CHROME_DEVEL_SANDBOX="/opt/tidalunar/bin/cef/chrome-sandbox"

# 4. Hand off to the actual binary in user home.
exec "$USER_DIR/tidalunar" "$@"
