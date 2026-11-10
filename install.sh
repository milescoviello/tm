#!/usr/bin/env bash
# Install tm for this user. Safe to run again: each step checks first.
#
#   ./install.sh                 install target/release/tm
#   ./install.sh path/to/tm      install a prebuilt binary
#
# 1. copies the binary to ~/.local/bin/tm (temp file + rename, so a running
#    `tm autosave` is never left holding a half-written file)
# 2. adds the tm block to ~/.tmux.conf (backup first) and reloads tmux
# 3. sets up the boot-time restore: a systemd user unit where there is a
#    systemd user manager, else an XDG autostart entry (desktop login)
set -euo pipefail
cd "$(dirname "$0")"
bin=${1:-target/release/tm}
dest=$HOME/.local/bin/tm

if [ ! -x "$bin" ]; then
    echo "no binary at $bin: run cargo build --release first, or pass one" >&2
    exit 1
fi
mkdir -p "$HOME/.local/bin"
install -m 755 "$bin" "$dest.new"
mv -f "$dest.new" "$dest"
echo "installed $dest ($("$dest" --version))"

conf=$HOME/.tmux.conf
if grep -qF '# >>> tm >>>' "$conf" 2>/dev/null; then
    echo "$conf already has the tm block"
else
    if [ -f "$conf" ]; then
        backup=$conf.before-tm.$(date +%Y%m%d-%H%M%S)
        cp "$conf" "$backup"
        echo "backed up $conf to $backup"
    fi
    cat contrib/tmux.conf.snippet >>"$conf"
    echo "added the tm block to $conf (prefix + t, autosave)"
fi
if tmux list-sessions >/dev/null 2>&1; then
    tmux source-file "$conf" && echo "reloaded the running tmux server's config"
fi

if [ -d /run/systemd/system ] && systemctl --user show-environment >/dev/null 2>&1; then
    mkdir -p "$HOME/.config/systemd/user"
    cp contrib/tm-restore.service "$HOME/.config/systemd/user/"
    systemctl --user daemon-reload
    systemctl --user enable tm-restore.service
    echo "enabled tm-restore.service (systemd user unit)"
    if ! loginctl show-user "$USER" -p Linger 2>/dev/null | grep -q '=yes'; then
        echo "note: lingering is off, so this runs at your first login, not at boot"
        echo "      (sudo loginctl enable-linger $USER makes it run at boot)"
    fi
else
    mkdir -p "$HOME/.config/autostart"
    cp contrib/tm-restore.desktop "$HOME/.config/autostart/"
    echo "installed ~/.config/autostart/tm-restore.desktop (runs at desktop login)"
    if [ -d "$HOME/.config/hypr" ]; then
        echo "Hyprland does not read XDG autostart: add this to hyprland.conf"
        echo "    exec-once = ~/.local/bin/tm restore --boot"
    fi
fi
echo "done. Type tm, or prefix + t inside tmux."
