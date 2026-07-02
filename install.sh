#!/usr/bin/env sh
# Build filescope from source and install it as a desktop application for the
# current user, so it appears in the Activities Overview / app grid and in
# "Open With" for folders.
#
#   ./install.sh              build in release and install for the current user
#   ./install.sh --uninstall  remove the installed files
#
# Everything goes under the per-user XDG dirs — no root, no system files.
set -eu

APP_ID="dev.filescope.Filescope"
BIN_NAME="filescope"

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$DATA_HOME/applications"
ICON_DIR="$DATA_HOME/icons/hicolor/scalable/apps"

DESKTOP_DEST="$APP_DIR/$APP_ID.desktop"
ICON_DEST="$ICON_DIR/$APP_ID.svg"
BIN_DEST="$BIN_DIR/$BIN_NAME"

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "-u" ]; then
    rm -f "$DESKTOP_DEST" "$ICON_DEST" "$BIN_DEST"
    refresh_caches
    echo "Removed filescope desktop entry, icon, and binary."
    exit 0
fi

echo "Building filescope (release)…"
( cd "$SCRIPT_DIR" && cargo build --release )
BIN_SRC="$SCRIPT_DIR/target/release/$BIN_NAME"
[ -x "$BIN_SRC" ] || { echo "error: build did not produce $BIN_SRC" >&2; exit 1; }

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

install -m 0755 "$BIN_SRC" "$BIN_DEST"
install -m 0644 "$SCRIPT_DIR/data/icons/$APP_ID.svg" "$ICON_DEST"

# Point the launcher at the absolute binary path so it works regardless of
# whether ~/.local/bin is on PATH inside the desktop session.
sed "s|^Exec=$BIN_NAME |Exec=$BIN_DEST |" \
    "$SCRIPT_DIR/data/$APP_ID.desktop" > "$DESKTOP_DEST"
chmod 0644 "$DESKTOP_DEST"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$DESKTOP_DEST" || true
fi

refresh_caches

echo "Installed filescope:"
echo "  binary   $BIN_DEST"
echo "  launcher $DESKTOP_DEST"
echo "  icon     $ICON_DEST"
echo
echo "Look for \"filescope\" in the Activities Overview (it may take a few"
echo "seconds to appear, or log out and back in)."
case ":${PATH:-}:" in
    *":$BIN_DIR:"*) : ;;
    *) echo "Note: $BIN_DIR is not on your PATH — add it to run \"filescope\" from a terminal." ;;
esac
