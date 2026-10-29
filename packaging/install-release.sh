#!/usr/bin/env sh
# Install a prebuilt filescope (from a release tarball) for the current user.
#
# This file is packaging/install-release.sh in the repo. Release packaging
# copies it into the tarball as install.sh, which is the command below.
#
# No build tools and no -dev packages are required — only the GTK4/libadwaita
# *runtime* plus VTE's GTK4 runtime (GTK 4.16+ / libadwaita 1.5+ / VTE 0.70+).
# The prebuilt x86_64 Linux binary also requires glibc 2.39+.
#
#   ./install.sh              install for the current user (under ~/.local)
#   ./install.sh --uninstall  remove it again
#
# You can also skip installing entirely and just run ./filescope from here.
# Shared GNOME search and portal discovery registrations need administrator access.
set -eu

APP_ID="dev.filescope.Filescope"
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

. "$DIR/update-layout"
. "$DIR/install-binary.sh"

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$DATA_HOME/applications"
ICON_DIR="$DATA_HOME/icons/hicolor/scalable/apps"
SEARCH_DIR="/usr/local/share/gnome-shell/search-providers"
SERVICE_DIR="$DATA_HOME/dbus-1/services"

DESKTOP_DEST="$APP_DIR/$APP_ID.desktop"
ICON_DEST="$ICON_DIR/$APP_ID.svg"
BIN_DEST="$BIN_DIR/$UPDATE_BINARY"
SEARCH_DEST="$SEARCH_DIR/$APP_ID.search-provider.ini"
SERVICE_DEST="$SERVICE_DIR/$APP_ID.service"
PICKER_SERVICE_DEST="$SERVICE_DIR/org.freedesktop.impl.portal.desktop.filescope.service"
PICKER_PORTAL_DEST="/usr/share/xdg-desktop-portal/portals/zz-filescope.portal"
LEGACY_SEARCH_DEST="$DATA_HOME/gnome-shell/search-providers/$APP_ID.search-provider.ini"

# Elevate only the shared registration, never the build or per-user files.
as_admin() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif [ ! -t 0 ] && command -v pkexec >/dev/null 2>&1; then
        pkexec "$@"
    else
        sudo "$@"
    fi
}

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "-u" ]; then
    if [ -d "$BIN_DIR" ]; then
        exec 9>> "$BIN_DIR/$UPDATE_LOCK"
        flock -x 9
    fi
    # Legacy binaries need no picker flag when this user never enabled it.
    case "${XDG_CONFIG_HOME:-}" in /*) PICKER_CONFIG_HOME="$XDG_CONFIG_HOME" ;; *) PICKER_CONFIG_HOME="$HOME/.config" ;; esac
    PICKER_RECORD="$PICKER_CONFIG_HOME/filescope/file-picker.json"
    if [ -e "$PICKER_RECORD" ] || [ -L "$PICKER_RECORD" ]; then
        [ -x "$BIN_DEST" ] || { echo "Cannot restore file picker preferences: reinstall Filescope before uninstalling." >&2; exit 1; }
        "$BIN_DEST" --disable-file-picker
        if [ -e "$PICKER_RECORD" ] || [ -L "$PICKER_RECORD" ]; then
            echo "File picker preferences were not restored; keeping Filescope installed." >&2
            exit 1
        fi
    fi
    if [ -e "$SEARCH_DEST" ]; then
        as_admin rm -f "$SEARCH_DEST"
    fi
    # Shared portal discovery stays for other users; this user's activation is removed.
    rm -f "$DESKTOP_DEST" "$ICON_DEST" "$BIN_DEST" "$LEGACY_SEARCH_DEST" "$SERVICE_DEST" "$PICKER_SERVICE_DEST"
    rm -f "$BIN_DIR/$UPDATE_RECEIPT" "$BIN_DIR/$UPDATE_BACKUP"
    refresh_caches
    echo "Removed filescope."
    exit 0
fi

[ -x "$DIR/filescope" ] || { echo "error: filescope binary not found next to this script" >&2; exit 1; }

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR" "$SERVICE_DIR"
install_user_binary "$DIR/filescope"
install -m 0644 "$DIR/$APP_ID.svg" "$ICON_DEST"
# Point the launcher at the absolute binary path so it works regardless of PATH.
sed "s|^Exec=filescope |Exec=$BIN_DEST |" "$DIR/$APP_ID.desktop" > "$DESKTOP_DEST"
chmod 0644 "$DESKTOP_DEST"
SEARCH_TEMP=$(mktemp)
PORTAL_TEMP=$(mktemp)
trap 'rm -f "$SEARCH_TEMP" "$PORTAL_TEMP"' EXIT HUP INT TERM
cat > "$SEARCH_TEMP" <<EOF
[Shell Search Provider]
DesktopId=$APP_ID.desktop
BusName=$APP_ID
ObjectPath=/dev/filescope/SearchProvider
Version=2
EOF
printf '[D-BUS Service]\nName=%s\nExec="%s" --gapplication-service\n' "$APP_ID" "$BIN_DEST" > "$SERVICE_DEST"
chmod 0644 "$SERVICE_DEST"
printf '[D-BUS Service]\nName=org.freedesktop.impl.portal.desktop.filescope\nExec="%s" --file-picker-portal\n' "$BIN_DEST" > "$PICKER_SERVICE_DEST"
chmod 0644 "$PICKER_SERVICE_DEST"
cat > "$PORTAL_TEMP" <<EOF
[portal]
DBusName=org.freedesktop.impl.portal.desktop.filescope
Interfaces=org.freedesktop.impl.portal.FileChooser;
UseIn=filescope;
EOF
# GNOME scans system data dirs only; XDG_DATA_HOME is ignored for providers.
if ! cmp -s "$SEARCH_TEMP" "$SEARCH_DEST"; then
    echo "Registering GNOME search (administrator authentication required)…"
    as_admin install -D -m 0644 "$SEARCH_TEMP" "$SEARCH_DEST"
fi
# Prefix keeps existing gnome/gtk backends first in legacy alphabetical fallback.
if ! cmp -s "$PORTAL_TEMP" "$PICKER_PORTAL_DEST"; then
    echo "Registering file picker (administrator authentication required)…"
    as_admin install -D -m 0644 "$PORTAL_TEMP" "$PICKER_PORTAL_DEST"
fi
rm -f "$LEGACY_SEARCH_DEST"
refresh_caches

echo "filescope installed:"
echo "  binary   $BIN_DEST"
echo "  launcher $DESKTOP_DEST"
echo "  search   $SEARCH_DEST"
echo "  service  $SERVICE_DEST"
echo
echo "Launch it from the Activities Overview (search \"filescope\"), or run: filescope"
echo "Reopen Settings → Search, then toggle filescope off/on to reload Overview search."
echo "Optional file picker: turn on 'Use Filescope for Open/Save Dialogs' in the main menu."
case ":${PATH:-}:" in
    *":$BIN_DIR:"*) : ;;
    *) echo "Note: $BIN_DIR is not on your PATH — add it to run \"filescope\" from a terminal." ;;
esac
