#!/usr/bin/env bash
set -euo pipefail

# Where everything lands. Defaults to the real home directory, matching every
# path this project's own code assumes (config, state, token file). Override
# to install into a scratch location instead — handy for a dry run that
# proves the script works without touching the real $HOME:
#   PREFIX=/tmp/protector-test ./install.sh
# The colon in ${PREFIX:-$HOME} matters: it falls back on an empty PREFIX
# (e.g. `PREFIX= ./install.sh`) exactly the same as on an unset one, so this
# can never resolve to a root-relative path like /.local/bin.
PREFIX="${PREFIX:-$HOME}"
BIN_DIR="$PREFIX/.local/bin"

cd "$(dirname "${BASH_SOURCE[0]}")"

echo "==> Building release binary"
cargo build --release

echo "==> Installing binary to $BIN_DIR/protector"
install -Dm755 target/release/protector "$BIN_DIR/protector"

echo "==> Installing icons to $PREFIX/.local/share/icons/hicolor/scalable/apps/"
install -Dm644 assets/protector.svg "$PREFIX/.local/share/icons/hicolor/scalable/apps/protector.svg"
install -Dm644 assets/protector-attention.svg "$PREFIX/.local/share/icons/hicolor/scalable/apps/protector-attention.svg"

echo "==> Refreshing the icon cache"
gtk-update-icon-cache -f -t "$PREFIX/.local/share/icons/hicolor" 2>/dev/null || true

echo "==> Installing the systemd user unit to $PREFIX/.config/systemd/user/protector.service"
install -Dm644 assets/protector.service "$PREFIX/.config/systemd/user/protector.service"

# systemctl --user always talks to *this* login session's own manager and
# reads units from the real ~/.config/systemd/user, regardless of PREFIX — a
# daemon-reload only means something when PREFIX actually is the real home,
# so a dry run into a scratch PREFIX skips it rather than reloading units
# that have nothing to do with what was just installed.
if [ "$PREFIX" = "$HOME" ]; then
    echo "==> Reloading the systemd user daemon"
    systemctl --user daemon-reload
else
    echo "==> Skipping systemctl --user daemon-reload (PREFIX ($PREFIX) is not \$HOME — dry run)"
fi

# Only nagged about when it's actually true: on most GNOME desktops
# ~/.local/bin is already on PATH by the time a login shell starts, so this
# stays silent for the common case and only speaks up for the PATH that
# matters to running `protector` right afterwards — $BIN_DIR, not some
# other ~/.local/bin a differently-set PREFIX might imply.
PATH_NOTE=""
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        PATH_NOTE="
NOTE: $BIN_DIR is not on your PATH right now, so plain \`protector\` won't
be found until it is. Add this to your shell's startup file (~/.bashrc,
~/.zshrc, ...) and open a new terminal:

  export PATH=\"$BIN_DIR:\$PATH\"
"
        ;;
esac

cat <<EOF

Installed to $PREFIX.
$PATH_NOTE
Next steps:
  1. Edit $PREFIX/.config/protector/config.toml with your Google OAuth
     client id and secret. See README.md for the full Google Cloud setup.
  2. Run: protector login
  3. Protector does NOT start automatically yet. When you are ready for it
     to run every login, enable and start it yourself:

       systemctl --user enable --now protector.service

EOF
