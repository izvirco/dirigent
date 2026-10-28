#!/usr/bin/env bash
# Drive a headless dirigent_desktop for UI tests. Guide: testing.md.
#
#   scripts/ui.sh start [W H]            build, (re)launch on a private virtual display (default 1100x720)
#   scripts/ui.sh setup [DIR] [MODEL]    add DIR as a project, pick MODEL, open its new-thread page
#   scripts/ui.sh shot [path] [X Y W H]  screenshot, optionally cropped (default a fresh <session>/shot-XXXX.png, printed)
#   scripts/ui.sh click X Y [right]      click at window px (screenshot px == window px)
#   scripts/ui.sh move X Y               hover
#   scripts/ui.sh drag X1 Y1 X2 Y2       press, move, release
#   scripts/ui.sh scroll X Y N           N notches down at X Y (negative: up)
#   scripts/ui.sh type "text"            type text
#   scripts/ui.sh key KEY...             key chords, xdotool names: Return Escape ctrl+n shift+Return
#   scripts/ui.sh wait-idle [SECS]       until no thread runs or has async_bash jobs (default 300 s)
#   scripts/ui.sh rpc JSON [EXPR]        one protocol request to the session's service, print the reply
#   scripts/ui.sh run CMD...             CMD with the dev-shell tools, DISPLAY, UI_DIR and UI_PORT set
#   scripts/ui.sh log [N]                last N (40) lines of the app's stdout/stderr
#   scripts/ui.sh stop                   kill the app and its display
#   scripts/ui.sh reset                  stop and wipe the session's data (fresh service + desktop dbs)
#
# Each session (UI_SESSION; default: the last 8 chars of the calling agent's thread id, else
# "default") gets its own Xvfb display, service port, data and config dirs under
# /tmp/dirigent-ui/<session>. Nothing touches the real desktop, mouse, keyboard, data, config
# or the service on :7777, and sessions can run in parallel.
set -euo pipefail
CALLER=$PWD
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# Tools (Xvfb, xdotool, imagemagick, sqlite, xclip, lavapipe, Lilex) and the app's runtime libs
# come from the dev shell. `nix develop` costs seconds per call, so its environment is cached
# until flake.nix / flake.lock change. Always sourced: a shell the caller entered before the
# flake last changed may lack newer tools.
NIX=(nix develop --option warn-dirty false "$ROOT" -c)
NIX_BUILD=("${NIX[@]}")
[ -n "${DIRIGENT_UI_VK_ICD:-}" ] && NIX_BUILD=() # already inside a dev shell: build right here
ENV=/tmp/dirigent-ui/devenv-$(cat flake.nix flake.lock | sha256sum | cut -c1-16).sh
if [ ! -s "$ENV" ]; then
  mkdir -p /tmp/dirigent-ui
  # shellcheck disable=SC2016
  "${NIX[@]}" bash -c 'printf "export PATH=%q LD_LIBRARY_PATH=%q DIRIGENT_UI_VK_ICD=%q FONTCONFIG_FILE=%q\n" "$PATH" "$LD_LIBRARY_PATH" "$DIRIGENT_UI_VK_ICD" "$FONTCONFIG_FILE"' >"$ENV.tmp"
  mv "$ENV.tmp" "$ENV"
fi
# shellcheck disable=SC1090
source "$ENV"

# One session per agent thread unless told otherwise, so parallel agents never share one.
if [ -n "${UI_SESSION:-}" ]; then
  SESSION=$UI_SESSION
elif [ -n "${PI_SESSION_ID:-}" ]; then
  SESSION=${PI_SESSION_ID: -8}
else
  SESSION=default
fi
DIR=/tmp/dirigent-ui/$SESSION
# The checkout's own target dir, the one `cargo test` uses. Never share one across checkouts:
# cargo maps every checkout's crates onto the same files and judges freshness by mtime, so
# one checkout would run another's build. The dev shell's sccache warms a fresh one.
BIN=$ROOT/target/debug/dirigent_desktop
mkdir -p "$DIR"

alive() { [ -f "$DIR/$1.pid" ] && kill -0 "$(cat "$DIR/$1.pid")" 2>/dev/null; }
# Both processes are started with setsid, so pid == process group: this also takes down
# the app's embedded service and pi runtime, and nothing outside the session.
kill_group() {
  if alive "$1"; then
    kill -TERM -- "-$(cat "$DIR/$1.pid")" 2>/dev/null || true
    for _ in $(seq 1 30); do alive "$1" || break; sleep 0.1; done
    alive "$1" && { kill -KILL -- "-$(cat "$DIR/$1.pid")" 2>/dev/null || true; }
  fi
  rm -f "$DIR/$1.pid"
}
need_display() {
  alive xvfb || { echo "no running session '$SESSION'; run: scripts/ui.sh start" >&2; exit 1; }
  DISPLAY=:$(cat "$DIR/display"); export DISPLAY
}
need_app() {
  alive app || { echo "the app of session '$SESSION' isn't running; run: scripts/ui.sh start" >&2; exit 1; }
}
settle() { sleep "${1:-0.15}"; }
# The service port: Xvfb hands every live session its own display number, so no two collide.
port() { echo $((7800 + $(cat "$DIR/display"))); }
rpc() { bun "$ROOT/scripts/ui-rpc.ts" "$(port)" "$@"; }

# Only errors are printed; the last line says what happened.
build() {
  [ -d node_modules ] || bun install --frozen-lockfile # a fresh workspace
  if ! "${NIX_BUILD[@]}" cargo build -q -p dirigent_desktop --message-format short 2>"$DIR/build.log"; then
    grep -E 'error' "$DIR/build.log" | grep -v '^error: could not compile' | head -30 || true
    echo "build failed, full output: $DIR/build.log" >&2
    exit 1
  fi
}

# Launch the app on the session's display (it must be up). Dirs as flags, not XDG_*, so the
# agents it runs keep the real git / jj config.
launch() {
  W=$(cut -dx -f1 "$DIR/size"); H=$(cut -dx -f2 "$DIR/size")
  DISPLAY=:$(cat "$DIR/display"); export DISPLAY
  PORT=$(port)
  ! ss -ltnH "sport = :$PORT" | grep -q . || { echo "port $PORT is taken; scripts/ui.sh stop, then start" >&2; exit 1; }
  mkdir -p "$DIR/data" "$DIR/config"
  env -u WAYLAND_DISPLAY -u DIRIGENT_URL -u DIRIGENT_DATA_DIR VK_ICD_FILENAMES="$DIRIGENT_UI_VK_ICD" \
    DIRIGENT_PORT="$PORT" DIRIGENT_BIND=127.0.0.1 DIRIGENT_RUNTIME="$ROOT/dirigent_runtime/src/index.ts" \
    setsid "$BIN" --data-dir "$DIR/data" --config-dir "$DIR/config" >"$DIR/app.log" 2>&1 </dev/null &
  echo $! >"$DIR/app.pid"
  WIN=$(timeout 20 xdotool search --sync --onlyvisible --class dirigent | head -1) ||
    { echo "window never appeared, see $DIR/app.log" >&2; exit 1; }
  xdotool windowmove "$WIN" 0 0 windowsize "$WIN" "$W" "$H"
  settle 1
}

case "${1:-}" in
  start)
    W=${2:-1100}; H=${3:-720}
    build
    kill_group app
    # The screen is exactly the window, so screenshot px == window px.
    if ! alive xvfb || [ "$(cat "$DIR/size" 2>/dev/null)" != "${W}x$H" ]; then
      kill_group xvfb
      rm -f "$DIR/display"
      setsid Xvfb -displayfd 3 -screen 0 "${W}x${H}x24" -nolisten tcp 3>"$DIR/display" >"$DIR/xvfb.log" 2>&1 </dev/null &
      echo $! >"$DIR/xvfb.pid"
      echo "${W}x$H" >"$DIR/size"
      for _ in $(seq 1 50); do [ -s "$DIR/display" ] && break; sleep 0.1; done
      [ -s "$DIR/display" ] || { echo "Xvfb failed, see $DIR/xvfb.log" >&2; exit 1; }
    fi
    launch
    echo "session '$SESSION' ready: ${W}x$H on $DISPLAY, service :$PORT, dir $DIR"
    ;;
  setup)
    # The desktop reads its cache (`app.rs`'s kv keys) at launch only, hence the restart.
    need_display; need_app
    PROJECT=${2:-$DIR/scratch}
    case $PROJECT in /*) ;; *) PROJECT=$CALLER/$PROJECT ;; esac
    MODEL=${3:-anthropic/claude-opus-5-5}
    mkdir -p "$PROJECT"
    PROJECT=$(cd "$PROJECT" && pwd)
    ID=$(P=$PROJECT rpc '{"type":"list_projects"}' 'r.projects.find(p => p.path === process.env.P)?.id ?? ""')
    [ -n "$ID" ] || ID=$(rpc "{\"type\":\"add_project\",\"path\":\"$PROJECT\"}" 'r.project.id')
    kill_group app
    sqlite3 "$DIR/data/desktop.db" "INSERT OR REPLACE INTO kv (key, value) VALUES ('last_model', '$MODEL'),
      ('last_target', 'local' || char(10) || '$ID'); DELETE FROM kv WHERE key IN ('pane', 'last_effort', 'last_isolate');
      DELETE FROM drafts WHERE host = ''; DELETE FROM draft_images WHERE host = '';"
    launch
    xdotool key ctrl+n
    settle 0.5
    echo "project $PROJECT ($ID), model $MODEL: new-thread page open, composer focused"
    ;;
  shot)
    need_display
    # A fresh name by default: a `read` racing the shot gets an empty file, not the previous frame.
    OUT=${2:-$(mktemp "$DIR/shot-XXXX.png")}
    settle 0.3
    if [ $# -ge 6 ]; then
      import -window root -crop "${5}x${6}+${3}+${4}" +repage "$OUT"
    else
      import -window root "$OUT"
    fi
    echo "$OUT"
    ;;
  click)
    need_display
    xdotool mousemove "$2" "$3" click "$([ "${4:-}" = right ] && echo 3 || echo 1)"
    settle
    ;;
  move)
    need_display
    xdotool mousemove "$2" "$3"
    settle
    ;;
  drag)
    need_display
    xdotool mousemove "$2" "$3" mousedown 1
    # Intermediate steps: gpui only sees a drag through motion events while pressed.
    for i in 1 2 3 4 5 6 7 8 9 10; do
      xdotool mousemove $(($2 + ($4 - $2) * i / 10)) $(($3 + ($5 - $3) * i / 10))
      sleep 0.02
    done
    xdotool mouseup 1
    settle
    ;;
  scroll)
    need_display
    N=$4; BUTTON=5
    if [ "$N" -lt 0 ]; then N=$((-N)); BUTTON=4; fi
    xdotool mousemove "$2" "$3" click --repeat "$N" --delay 30 "$BUTTON"
    settle
    ;;
  type)
    need_display
    xdotool type --delay 15 -- "$2"
    settle
    ;;
  key)
    need_display
    shift
    xdotool key --delay 30 -- "$@"
    settle
    ;;
  wait-idle)
    need_app
    # Idle for 5 polls in a row (~2 s): a prompt just sent takes a moment to show up as
    # running, and a parent picks its next turn up shortly after a sub-agent ends. A running
    # async_bash job counts as busy (its end wakes the thread).
    LIMIT=${2:-300}; IDLE=0; SECONDS=0
    [ "$LIMIT" -gt 0 ] 2>/dev/null || { echo "usage: scripts/ui.sh wait-idle [SECS]" >&2; exit 1; }
    while [ $IDLE -lt 5 ]; do
      BUSY=$(rpc '{"type":"list_threads"}' 'r.threads.filter(t => t.running_since != null || t.jobs > 0).map(t => t.id + " " + t.name).join(", ")')
      if [ -n "$BUSY" ]; then IDLE=0; else IDLE=$((IDLE + 1)); fi
      [ $SECONDS -lt "$LIMIT" ] || { echo "still busy after ${LIMIT}s: $BUSY" >&2; exit 1; }
      sleep 0.3
    done
    echo "idle after ${SECONDS}s"
    ;;
  rpc)
    need_app
    [ $# -ge 2 ] || { echo "usage: scripts/ui.sh rpc JSON [EXPR]" >&2; exit 1; }
    rpc "$2" "${3:-}"
    ;;
  run)
    shift
    [ $# -ge 1 ] || { echo "usage: scripts/ui.sh run CMD..." >&2; exit 1; }
    [ -s "$DIR/display" ] && { DISPLAY=:$(cat "$DIR/display"); export DISPLAY; }
    UI_DIR=$DIR UI_PORT=$(port 2>/dev/null || true) exec "$@"
    ;;
  log)
    tail -n "${2:-40}" "$DIR/app.log"
    ;;
  stop)
    kill_group app
    kill_group xvfb
    ;;
  reset)
    kill_group app
    kill_group xvfb
    rm -rf "$DIR"
    ;;
  *)
    sed -n '2,23p' "$0"
    exit 1
    ;;
esac
