# tm

A tmux manager for people who keep a lot of sessions open, most of them
running [Claude Code](https://claude.com/claude-code). Type `tm` (or press
`prefix + t` inside tmux) to see every session, what is running in it, and
what that costs. tm also snapshots the whole server every 15 minutes, and
after a reboot it brings everything back: windows, splits, folders,
scrollback, and each Claude conversation, resumed by its exact session id.

Linux only (it reads `/proc`). Written in Rust with ratatui. Built with a lot
of help from Claude Code.

## The live view

```
 tm   11 sessions  ·  9 claude  ·  2 busy                      saved 4m ago

❯ ⠹ 9   API rate limiter         busy   ~/api main       1s   3%  540M │ 9-api-rate-limiter   window 1 claude
  ○ 10  Release notes draft      idle   ~               22m   0%  135M │ claude busy   claude-opus-5-5   5f2c81a0
  ▪ 11  htop                            ~                0s   2%    5M │ ~/server  main   up 3d  ·  active 1s ago
  · 12  bash                            /tmp             4h   0%    4M │ ─────────────────────────────────────
                                                                       │ (live picture of the selected pane)
 enter open   n new   r rename   x kill   s save   S snapshots   / filter   ? help   q quit
```

Each row is a session: a status mark (spinner = Claude working, ○ = Claude
idle, ▪ = a program, · = a shell), its number, the Claude session's title or
the program running, Claude's state, folder and git branch, time since last
output, and CPU and memory for everything running in it. Sessions with
several windows list each window under them. The right side is a live
capture of the selected pane, with its model and session id.

Keys: `enter` (or click) switches to the session, `1` `3` jumps to session 13,
`n` starts something new (Claude in a folder, a past Claude picked in
[mnemosyne](https://github.com/milescoviello/mnemosyne), a shell, or ssh to a
host from `~/.ssh/config`), `r` renames, `x` kills (asks first, and says so if
Claude is busy in it), `s` saves a snapshot, `S` browses snapshots, `/`
filters, `p` hides the preview, `M` turns the mouse off so the terminal can
select text.

## Snapshots and restore

`tm autosave` (started from `~/.tmux.conf`) takes a snapshot every 15
minutes. A snapshot is written only when something changed, and the newest
50 are kept in `~/.local/state/tm/snapshots/`. Each one records:

- every session, window (name, index, exact layout) and pane (folder, shell);
- for a Claude pane: the session id, the model it was last using, and the
  flags it was started with (`--model`, `--permission-mode`,
  `--dangerously-skip-permissions`, `--add-dir`, ...), but never a prompt
  given on the command line or an old `--resume`/`--continue`;
- for any other pane: the last 2000 lines of scrollback, and the program,
  if it is one that is safe to start again (viewers and editors such as
  htop, btop, nvim, less, `tail -f`, `journalctl -f`, an interactive `ssh`).
  Anything else, like a build or a script, comes back as a shell in the
  same folder.

At boot, `tm restore --boot` (a systemd user unit, or an XDG autostart entry
on systems without one) rebuilds the newest snapshot if tmux has no sessions
yet. Commands are typed into each pane's login shell, so PATH is right and
quitting a program leaves you at a prompt. Claude sessions start a couple of
seconds apart. A session that is already running, or whose Claude
conversation is already open anywhere, is skipped, so two processes never
write to one transcript.

```
tm restore --dry-run        what would happen
tm restore --only 9,12      just those sessions
tm restore --snapshot 2026  an older snapshot (tm snapshots lists them)
```

### How tm knows which Claude is which

Claude Code writes `~/.claude/sessions/<pid>.json` for each running process,
with its session id. tm matches those files by pid against each pane's own
process tree, and checks the recorded start time so a reused pid can't claim
a dead session. Older Claude versions without those files fall back to a
`--resume <id>` on the command line, then to matching the pane title against
the transcript's title.

## Session names

A session named with a bare number (`9`) is renamed `9-<claude title>`
(`9-api-rate-limiter`) and follows the title as it changes. A name you choose
is never touched. Because names start with the number, `tm 9` switches to it.
Plain `tmux attach -t 9` still works too, except where one number is the start
of another (`-t 1` when `10-...` exists); use `tm 1` or `-t 1-` there.

## Install

```sh
cargo build --release
./install.sh
```

This copies the binary to `~/.local/bin/tm`, appends a marked block to
`~/.tmux.conf` (after a backup) for `prefix + t` and autosave, reloads tmux,
and enables the boot restore. Delete the block and
`~/.config/systemd/user/tm-restore.service` (or
`~/.config/autostart/tm-restore.desktop`) to undo it. Hyprland doesn't read
XDG autostart; add `exec-once = ~/.local/bin/tm restore --boot` there.

For another machine, build a static binary once and copy it:

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

## Commands

```
tm                  the live view
tm <n|name|words>   switch to a session by number, name, or a piece of its title
tm ls               the same information as text, for scripts and phones
tm save             snapshot now
tm restore          bring back the newest snapshot (see above)
tm snapshots        list snapshots
tm write-config     write a commented ~/.config/tm/config.toml
```

`--socket NAME|PATH` (or `TM_TMUX_SOCKET`) points tm at another tmux server.

## Configuration

`tm write-config` writes `~/.config/tm/config.toml` with every setting and its
default: autosave interval, snapshots kept, scrollback lines, flags for new
Claude sessions, the restore stagger, auto-naming, and extra programs that
may (or may never) be started again.

## Tests

```sh
cargo test          # unit tests
tools/itest.sh      # save, kill the server, restore, compare; on a private
                    # tmux server with a fake HOME and a fake claude
tools/shot.py       # render a pane to PNG, for checking the TUI
```

## License

MIT
