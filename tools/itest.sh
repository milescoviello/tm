#!/usr/bin/env bash
# End-to-end test of save and restore on a private tmux server with a fake
# HOME, so nothing here can touch a real server, real snapshots or real
# Claude sessions. Builds a fake-claude session, a split window with
# scrollback and a viewer, a fish pane, and a hand-named session; saves;
# kills the server; restores; and checks that it all came back.
#
#   tools/itest.sh            (uses target/release/tm)
#   TM_BIN=path tools/itest.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tm=${TM_BIN:-$here/../target/release/tm}
tm=$(realpath "$tm")
root=$(mktemp -d "${TMPDIR:-/tmp}/tm-itest.XXXXXX")
sock=tm-itest-$$
export HOME=$root/home
mkdir -p "$HOME" "$root/bin" "$root/work/a" "$root/work/b c"
cp "$here/fake-claude" "$root/bin/claude"
chmod +x "$root/bin/claude"
export PATH=$root/bin:/usr/local/bin:/usr/bin:/bin
unset TMUX TMUX_PANE CLAUDECODE TM_STATE_DIR TM_CLAUDE_DIR TM_CONFIG \
    XDG_STATE_HOME XDG_CONFIG_HOME CLAUDE_CONFIG_DIR TM_TMUX_SOCKET
export SHELL=/bin/bash
printf 'export PATH=%s\nPS1="$ "\n' "$PATH" >"$HOME/.bash_profile"
mkdir -p "$HOME/.config/tm"
printf 'claude_stagger_secs = 1\n' >"$HOME/.config/tm/config.toml"

t() { tmux -L "$sock" "$@"; }
T() { "$tm" --socket "$sock" "$@"; }
cleanup() {
    t kill-server 2>/dev/null || true
    rm -rf "$root"
}
trap cleanup EXIT
fails=0
ok() { echo "ok    $*"; }
bad() {
    echo "FAIL  $*"
    fails=$((fails + 1))
}
check() { # check "what" command...
    local what=$1
    shift
    if "$@"; then ok "$what"; else bad "$what"; fi
}
wait_for() { # wait_for seconds command...
    local n=$(($1 * 5))
    shift
    while ((n-- > 0)); do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}
proj() { # proj cwd sid title model
    local d="$HOME/.claude/projects/$(printf '%s' "$1" | sed 's/[^A-Za-z0-9]/-/g')"
    mkdir -p "$d"
    printf '%s\n' "{\"type\":\"ai-title\",\"aiTitle\":\"$3\"}" \
        "{\"message\":{\"role\":\"assistant\",\"model\":\"$4\"}}" >"$d/$2.jsonl"
}
pane_has() { t capture-pane -p -J -S -200 -t "$1" | grep -qF -- "$2"; }
registered() { [ "$(ls "$HOME/.claude/sessions"/*.json 2>/dev/null | wc -l)" -ge "$1" ]; }

S1=11111111-1111-4111-8111-111111111111
S3=33333333-3333-4333-8333-333333333333
proj "$root/work/a" $S1 "Alpha task title" model-alpha
proj "$root/work/b c" $S3 "Gamma in fish" model-gamma

# ---- build -----------------------------------------------------------------
t -f /dev/null new-session -d -s 1 -x 120 -y 30 -c "$root/work/a"
t send-keys -t =1: "FAKE_TITLE='Alpha task title' claude --resume $S1 --model old-model --dangerously-skip-permissions 'a prompt'" Enter

t new-session -d -s 2 -x 120 -y 30 -c "$root/work/a"
t send-keys -t =2: "echo hello-scrollback-marker" Enter
right=$(t split-window -h -P -F '#{pane_id}' -t =2: -c "$root/work/b c")
t send-keys -t "$right" "tail -f /dev/null" Enter
logs=$(t new-window -P -F '#{pane_id}' -t =2: -n logs -c /)
t send-keys -t "$logs" "less /etc/hostname" Enter
sl=$(t new-window -P -F '#{pane_id}' -t =2: -n busywork -c /tmp)
t send-keys -t "$sl" "sleep 9999" Enter
t select-window -t =2:0

if command -v fish >/dev/null; then
    t new-session -d -s 3 -x 100 -y 25 -c "$root/work/b c" fish -l
    sleep 1
    t send-keys -t =3: "set -x FAKE_TITLE 'Gamma in fish'; claude --resume $S3" Enter
    want=2
else
    echo "skip  fish not installed"
    want=1
fi
t new-session -d -s notes -x 100 -y 25 -c /tmp

check "fake claudes registered" wait_for 5 registered $want
sleep 1

# ---- live view -------------------------------------------------------------
ls_out=$(T ls)
echo "$ls_out" | sed 's/^/      /'
check "ls shows the claude title" grep -q "Alpha task title" <<<"$ls_out"
check "ls shows tail as a program" grep -q "tail -f /dev/null" <<<"$ls_out"

# ---- save ------------------------------------------------------------------
T save
names=$(t list-sessions -F '#{session_name}' | sort | tr '\n' ' ')
echo "      sessions now: $names"
check "numbered claude session got its title" grep -q "1-alpha-task-title" <<<"$names"
check "hand-named session kept its name" grep -qw "notes" <<<"$names"
check "numbered session without claude kept its number" grep -qw "2" <<<"$names"
[ $want = 2 ] && check "fish claude session renamed" grep -q "3-gamma-fish" <<<"$names"
check "second save writes nothing" bash -c "'$tm' --socket '$sock' save | grep -q 'nothing changed'"

snap=$(ls "$HOME/.local/state/tm/snapshots/"*.json | tail -1)
check "snapshot is private" test "$(stat -c %a "$snap")" = 600
check "claude id saved" grep -q "$S1" "$snap"
check "model from transcript saved" grep -q '"model-alpha"' "$snap"
check "prompt not carried" bash -c "! grep -q 'a prompt' '$snap'"
check "viewer saved as program" grep -q '"less"' "$snap"
check "sleep not saved as program" bash -c "! grep -q '\"sleep\"' '$snap'"
check "scrollback file written" bash -c "grep -rqF hello-scrollback-marker '$HOME/.local/state/tm/scrollback/'"

before_layout=$(t list-windows -a -F '#{session_name}:#{window_index} #{window_name} #{window_panes}' | sort)
before_paths=$(t list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_current_path}' | sort)
before_sizes=$(t list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height}' | sort)

# ---- dry run ---------------------------------------------------------------
dry=$(T restore --dry-run)
check "dry run skips running sessions" grep -q "already running" <<<"$dry"
dry=$(T restore --dry-run --ignore-running)
echo "$dry" | sed 's/^/      /'
check "dry run plans the resume" grep -q -- "--resume $S1 --model model-alpha --dangerously-skip-permissions" <<<"$dry"

# ---- reboot ----------------------------------------------------------------
t kill-server
sleep 0.5
check "server is gone" bash -c "! tmux -L '$sock' list-sessions 2>/dev/null"
T restore --boot
check "boot restore made sessions" wait_for 5 bash -c "tmux -L '$sock' has-session -t =1-alpha-task-title"
sleep $((want + 2))

after_layout=$(t list-windows -a -F '#{session_name}:#{window_index} #{window_name} #{window_panes}' | sort)
after_paths=$(t list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_current_path}' | sort)
after_sizes=$(t list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height}' | sort)
check "same windows and pane counts" test "$before_layout" = "$after_layout"
check "same folders per pane" test "$before_paths" = "$after_paths"
check "same pane sizes" test "$before_sizes" = "$after_sizes"
[ "$before_layout" = "$after_layout" ] || diff <(echo "$before_layout") <(echo "$after_layout") || true
[ "$before_sizes" = "$after_sizes" ] || diff <(echo "$before_sizes") <(echo "$after_sizes") || true

check "claude resumed with id, transcript model and flags" wait_for 5 pane_has "=1-alpha-task-title:" "args: --resume $S1 --model model-alpha --dangerously-skip-permissions"
check "scrollback printed back" pane_has "=2:0.0" "hello-scrollback-marker"
check "tail running again" bash -c "[ \"\$(tmux -L '$sock' display -p -t =2:0.1 '#{pane_current_command}')\" = tail ]"
check "less running again" bash -c "[ \"\$(tmux -L '$sock' display -p -t =2:logs '#{pane_current_command}')\" = less ]"
check "sleep came back as a shell" bash -c "[ \"\$(tmux -L '$sock' display -p -t =2:busywork '#{pane_current_command}')\" = bash ]"
check "active window kept" bash -c "[ \"\$(tmux -L '$sock' display -p -t =2: '#{window_index}')\" = 0 ]"
if [ $want = 2 ]; then
    check "fish pane runs fish" bash -c "[ \"\$(tmux -L '$sock' display -p -t =3-gamma-fish: '#{pane_current_command}')\" = claude ] || tmux -L '$sock' capture-pane -p -t =3-gamma-fish: | grep -q 'fake claude'"
    check "claude resumed inside fish" wait_for 5 pane_has "=3-gamma-fish:" "fake claude $S3"
fi
check "no CLAUDECODE leaked into the new server" bash -c "! tmux -L '$sock' show-environment -g 2>/dev/null | grep -q '^CLAUDECODE='"
check "auto-name marker restored" bash -c "[ \"\$(tmux -L '$sock' show-options -v -t =1-alpha-task-title: @tm-auto)\" = 1-alpha-task-title ]"

again=$(T restore)
check "second restore skips everything" grep -q "0 made" <<<"$again"
boot2=$(T restore --boot)
check "boot restore with sessions does nothing" grep -q "does nothing" <<<"$boot2"

# ---- naming ----------------------------------------------------------------
# the restored fake claude has a new title now: tm follows it...
T save -q
check "auto name follows a new title" bash -c "tmux -L '$sock' has-session -t =1-fake-11111111"
# ...until a person names the session
t rename-session -t =1-fake-11111111 mine
T save -q
check "a session renamed by hand stays renamed" bash -c "tmux -L '$sock' has-session -t =mine"

# ---- attach by number ------------------------------------------------------
resolves() { local out; out=$(T "$1" 2>&1 </dev/null || true); ! grep -q "no session matches" <<<"$out"; }
check "tm <number> finds the session" resolves 3
check "tm <words> finds the session by title" resolves "Fake 1111"
check "tm <junk> says so" bash -c "'$tm' --socket '$sock' zzqq 2>&1 | grep -q 'no session matches'"

# ---- autosave --------------------------------------------------------------
spid=$(t display -p '#{pid}')
T autosave --server-pid "$spid" &
first=$!
sleep 0.5
T autosave --server-pid "$spid" &
second=$!
sleep 1
check "second autosave exits at once" bash -c "! kill -0 $second 2>/dev/null"
check "first autosave keeps running" kill -0 $first
t kill-server
check "autosave exits with its server" wait_for 8 bash -c "! kill -0 $first 2>/dev/null"

echo
if ((fails)); then
    echo "$fails check(s) failed"
    exit 1
fi
echo "all checks passed"
