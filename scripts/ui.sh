#!/usr/bin/env bash
# Run a debug dirigent_desktop to test, on your private screen. Guide: testing.md.
#
#   scripts/ui.sh start DISPLAY          build, (re)launch on your private screen, filling it (DISPLAY as the
#                                        `computer` tool named it, e.g. :3)
#   scripts/ui.sh setup [DIR] [MODEL]    add DIR as a project, pick MODEL, restart the app
#   scripts/ui.sh wait-idle [SECS]       until no thread runs or has async_bash jobs (default 300 s)
#   scripts/ui.sh rpc JSON [EXPR]        one protocol request to the session's service, print the reply
#   scripts/ui.sh run CMD...             CMD with the dev-shell tools, DISPLAY, UI_DIR and UI_PORT set
#   scripts/ui.sh log [N]                last N (40) lines of the app's stdout/stderr
#   scripts/ui.sh stop                   kill the app
#   scripts/ui.sh reset                  stop and wipe the session's data (fresh service + desktop dbs)
#
# Seeing and operating the app is the `computer` tool's: open a private screen with it, `start` the app
# there, and use the screen's tools. Only ever a private screen: `start` refuses a display that is not an
# Xvfb of yours, so the app never opens on a desktop.
#
# Each session (UI_SESSION; default: the last 8 chars of the calling agent's thread id, else
# "default") gets its own service port, data and config dirs under /tmp/dirigent-ui-<user>/<session>.
# Nothing touches the real data, config or the service on :7777, and sessions can run in parallel,
# each on its own screen.
set -euo pipefail
CALLER=$PWD
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# Everything the script writes goes under one directory per user: /tmp is shared, and a
# directory another user made can't be written to. Nor trusted: `$ENV` below is sourced.
BASE=/tmp/dirigent-ui-$(id -un)
mkdir -p "$BASE"
[ -O "$BASE" ] || { echo "$BASE belongs to another user" >&2; exit 1; }

# Tools (xdotool, sqlite, imagemagick, lavapipe, setsid, ss, pgrep; Lilex for the build)
# and the app's runtime libs come from the dev shell, not from the caller's PATH, which differs
# from user to user. `nix develop` costs seconds per call, so its environment is cached
# until flake.nix / flake.lock change. Always sourced: a shell the caller entered before the
# flake last changed may lack newer tools.
NIX=(nix develop --option warn-dirty false "$ROOT" -c)
NIX_BUILD=("${NIX[@]}")
[ -n "${DIRIGENT_UI_VK_ICD:-}" ] && NIX_BUILD=() # already inside a dev shell: build right here
ENV=$BASE/devenv-$(cat flake.nix flake.lock | sha256sum | cut -c1-16).sh
if [ ! -s "$ENV" ]; then
  # shellcheck disable=SC2016
  "${NIX[@]}" bash -c 'printf "export PATH=%q LD_LIBRARY_PATH=%q DIRIGENT_UI_VK_ICD=%q DIRIGENT_LILEX_DIR=%q\n" "$PATH" "$LD_LIBRARY_PATH" "$DIRIGENT_UI_VK_ICD" "$DIRIGENT_LILEX_DIR"' >"$ENV.tmp"
  mv "$ENV.tmp" "$ENV"
fi
# shellcheck disable=SC1090
source "$ENV"
# `UI_PATH`: directories put before the dev shell's PATH, for fakes the app should run instead
# (`ssh`, `xdg-open`; see testing.md).
[ -n "${UI_PATH:-}" ] && PATH="$UI_PATH:$PATH"

# One session per agent thread unless told otherwise, so parallel agents never share one.
if [ -n "${UI_SESSION:-}" ]; then
  SESSION=$UI_SESSION
elif [ -n "${PI_SESSION_ID:-}" ]; then
  SESSION=${PI_SESSION_ID: -8}
else
  SESSION=default
fi
DIR=$BASE/$SESSION
# The checkout's own target dir, the one `cargo test` uses. Never share one across checkouts:
# cargo maps every checkout's crates onto the same files and judges freshness by mtime, so
# one checkout would run another's build. The dev shell's sccache warms a fresh one.
BIN=$ROOT/target/debug/dirigent_desktop
mkdir -p "$DIR"

# The session's app by its command line (absolute binary, then the session's data dir): an
# app that restarted itself (`update.rs`) has a new pid and process group. An update installs
# into the session's own prefix (DIRIGENT_PREFIX, never ~/.local) and starts from there.
PREFIX=$DIR/prefix
APP_CMD="^($BIN|$PREFIX/lib/dirigent(-nightly)?/dirigent) --data-dir $DIR/data( |$)"
alive() {
  [ -f "$DIR/app.pid" ] && kill -0 "$(cat "$DIR/app.pid")" 2>/dev/null && return
  pgrep -f "$APP_CMD" >/dev/null
}
# The app is started with setsid, so pid == process group: this also takes down its embedded
# service and pi runtime, and nothing outside the session. A restarted app is signalled by
# its command line; its runtime follows it out (stdin closes).
kill_app() {
  if alive; then
    [ -f "$DIR/app.pid" ] && { kill -TERM -- "-$(cat "$DIR/app.pid")" 2>/dev/null || true; }
    pkill -TERM -f "$APP_CMD" || true
    for _ in $(seq 1 30); do alive || break; sleep 0.1; done
    [ -f "$DIR/app.pid" ] && { kill -KILL -- "-$(cat "$DIR/app.pid")" 2>/dev/null || true; }
    pkill -KILL -f "$APP_CMD" || true
  fi
  rm -f "$DIR/app.pid"
}
# The session's display is up and a private screen: an Xvfb of this user's serves it.
private_screen() {
  ss -Hxlp 2>/dev/null | grep -F "/tmp/.X11-unix/X$1 " | grep -q '"Xvfb"'
}
need_display() {
  [ -s "$DIR/display" ] || { echo "no session '$SESSION'; run: scripts/ui.sh start DISPLAY" >&2; exit 1; }
  private_screen "$(cat "$DIR/display")" ||
    { echo "session '$SESSION': its screen :$(cat "$DIR/display") is gone; open your private screen again and start" >&2; exit 1; }
  DISPLAY=:$(cat "$DIR/display"); export DISPLAY
}
need_app() {
  alive || { echo "the app of session '$SESSION' isn't running; run: scripts/ui.sh start DISPLAY" >&2; exit 1; }
}
# The service port: every live private screen has its own display number, so no two collide.
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

# The first-run setup (`onboarding.rs`) would cover the app: a session has it marked done,
# and a fresh one this version kept too (else the app takes the db for an old install and
# opens the changelog). UI_ONBOARDING=1 brings it back.
onboarding() {
  local sql version
  version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -1)
  if [ "${UI_ONBOARDING:-}" = 1 ]; then
    sql="DELETE FROM kv WHERE key = 'onboarded';"
  else
    sql="INSERT OR IGNORE INTO kv (key, value) VALUES ('onboarded', '1'), ('last_version', '$version');"
  fi
  sqlite3 "$DIR/data/desktop.db" "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL); $sql"
}

# Launch the app on the session's display (it must be up), filling it: the screen's pixels are
# the window's. Dirs as flags, not XDG_*, so the agents it runs keep the real git / jj config.
# Not the caller's PI_SESSION_ID or UI_SESSION: the app's own agents would take them for
# theirs (a nix dev shell they capture carries them into their commands), and their
# `ui.sh` would act on this session, the app they run in.
launch() {
  DISPLAY=:$(cat "$DIR/display"); export DISPLAY
  read -r W H < <(xdotool getdisplaygeometry)
  PORT=$(port)
  ! ss -ltnH "sport = :$PORT" | grep -q . || { echo "port $PORT is taken: by another session's app on this screen? One app per screen" >&2; exit 1; }
  mkdir -p "$DIR/data" "$DIR/config"
  onboarding
  env -u WAYLAND_DISPLAY -u DIRIGENT_URL -u DIRIGENT_DATA_DIR -u PI_SESSION_ID -u UI_SESSION \
    VK_ICD_FILENAMES="$DIRIGENT_UI_VK_ICD" \
    DIRIGENT_PORT="$PORT" DIRIGENT_BIND=127.0.0.1 DIRIGENT_RUNTIME="$ROOT/dirigent_runtime/src/index.ts" \
    DIRIGENT_PREFIX="$PREFIX" setsid "$BIN" --data-dir "$DIR/data" --config-dir "$DIR/config" >"$DIR/app.log" 2>&1 </dev/null &
  echo $! >"$DIR/app.pid"
  WIN=$(timeout 20 xdotool search --sync --onlyvisible --class dirigent | head -1) ||
    { echo "window never appeared, see $DIR/app.log" >&2; exit 1; }
  xdotool windowmove "$WIN" 0 0 windowsize "$WIN" "$W" "$H"
  sleep 1
}

case "${1:-}" in
  start)
    # No default: the caller's DISPLAY is the user's desktop.
    D=${2:-}; D=${D#:}
    private_screen "$D" || {
      echo "not a private screen: '${2:-}'. Open one with the computer tool (screen: private) and pass the display it names: scripts/ui.sh start :N" >&2
      exit 1
    }
    build
    kill_app
    echo "$D" >"$DIR/display"
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
    kill_app
    sqlite3 "$DIR/data/desktop.db" "INSERT OR REPLACE INTO kv (key, value) VALUES ('last_model', '$MODEL'),
      ('last_target', 'local' || char(10) || '$ID'); DELETE FROM kv WHERE key IN ('pane', 'last_effort', 'last_isolate');
      DELETE FROM drafts WHERE host = ''; DELETE FROM draft_images WHERE host = '';"
    launch
    echo "project $PROJECT ($ID), model $MODEL: on the Inbox; ctrl+n opens a new thread in the project"
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
    # Never the caller's DISPLAY, the user's desktop: what CMD opens is on the session's screen or nowhere.
    unset DISPLAY
    if [ -s "$DIR/display" ] && private_screen "$(cat "$DIR/display")"; then DISPLAY=:$(cat "$DIR/display"); export DISPLAY; fi
    UI_DIR=$DIR UI_PORT=$(port 2>/dev/null || true) exec "$@"
    ;;
  log)
    tail -n "${2:-40}" "$DIR/app.log"
    ;;
  stop)
    kill_app
    ;;
  reset)
    kill_app
    rm -rf "$DIR"
    ;;
  *)
    sed -n '2,21p' "$0"
    exit 1
    ;;
esac
