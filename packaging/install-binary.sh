# Shared by source/release installers. update-layout defines the updater contract.
install_user_binary() (
    set -eu
    mkdir -p "$BIN_DIR"
    # The application updater takes the same advisory lock.
    flock -x 9
    update_stage=$(mktemp -d "$BIN_DIR/.filescope-install.XXXXXX")
    trap 'rm -rf "$update_stage"' EXIT HUP INT TERM
    install -m 0755 "$1" "$update_stage/binary"
    update_old=''
    if [ -e "$BIN_DEST" ] || [ -L "$BIN_DEST" ]; then
        [ -f "$BIN_DEST" ] && [ ! -L "$BIN_DEST" ] || { echo 'Refusing to replace a non-regular installation' >&2; exit 1; }
        update_old=$(sha256sum "$BIN_DEST" | cut -d ' ' -f 1)
    fi
    update_new=$(sha256sum "$update_stage/binary" | cut -d ' ' -f 1)
    printf 'filescope-user-install-v1\n%s\n%s\n' "$update_old" "$update_new" > "$update_stage/receipt"
    chmod 0644 "$update_stage/receipt"
    mv -fT "$update_stage/receipt" "$BIN_DIR/$UPDATE_RECEIPT"
    mv -fT "$update_stage/binary" "$BIN_DEST"
) 9>> "$BIN_DIR/$UPDATE_LOCK"
