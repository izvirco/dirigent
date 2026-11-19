#!/usr/bin/env bash
# Run a debug dirigent_desktop to test, on your private screen. Guide: testing.md.
#
#   scripts/ui.sh start SCREEN           build, (re)launch on your private screen (SCREEN: the
#                                        WAYLAND_DISPLAY the `computer` tool named, /tmp/dirigent-screen-…/wayland-1)
#   scripts/ui.sh setup [DIR] [MODEL]    add DIR as a project, pick MODEL, restart the app
#   scripts/ui.sh wait-idle [SECS]       until no thread runs or has async_bash jobs (default 300 s)
#   scripts/ui.sh rpc JSON [EXPR]        one protocol request to the session's service, print the reply
#   scripts/ui.sh run CMD...             CMD with the dev-shell tools, the screen's WAYLAND_DISPLAY and SWAYSOCK,
#                                        UI_DIR and UI_PORT set
#   scripts/ui.sh log [N]                last N (40) lines of the app's stdout/stderr
#   scripts/ui.sh stop                   kill the app
#   scripts/ui.sh reset                  stop and wipe the session's data (fresh service + desktop dbs)
#
# Seeing and operating the app is the `computer` tool's: open a private screen with it, `start` the app
# there, and use the screen's tools. Only ever a private screen: `start` refuses a display that is not one
# the tool made, so the app never opens on a desktop.
#
# Each session (UI_SESSION; default: the last 8 chars of the calling agent's thread id, else
# "default") gets its own service port, data and config dirs under /tmp/dirigent-ui-<user>/<session>.
# Nothing touches the real data, config or the service on :7777, and sessions can run in parallel,
# each on its own screen.
set -euo pipefail
CALLER=$PWD
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

BASE=/tmp/dirigent-ui-$(id -un)
mkdir -p "$BASE"
[ -O "$BASE" ] || { echo "$BASE belongs to another user" >&2; exit 1; }

NIX=(nix develop --option warn-dirty false "$ROOT" -c)
NIX_BUILD=("${NIX[@]}")
[ -n "${DIRIGENT_DEV_SHELL:-}" ] && NIX_BUILD=()
ENV=$BASE/devenv-$(cat flake.nix flake.lock | sha256sum | cut -c1-16).sh
if [ ! -s "$ENV" ]; then
  # shellcheck disable=SC2016
  "${NIX[@]}" bash -c 'printf "export PATH=%q LD_LIBRARY_PATH=%q DIRIGENT_UI_VK_ICD=%q DIRIGENT_LILEX_DIR=%q\n" "$PATH" "$LD_LIBRARY_PATH" "$DIRIGENT_UI_VK_ICD" "$DIRIGENT_LILEX_DIR"' >"$ENV.tmp"
  mv "$ENV.tmp" "$ENV"
fi
# shellcheck disable=SC1090
source "$ENV"
[ -n "${UI_PATH:-}" ] && PATH="$UI_PATH:$PATH"

if [ -n "${UI_SESSION:-}" ]; then
  SESSION=$UI_SESSION
elif [ -n "${DIRIGENT_THREAD_ID:-}" ]; then
  SESSION=${DIRIGENT_THREAD_ID: -8}
else
  SESSION=default
fi
DIR=$BASE/$SESSION
BIN=$ROOT/target/debug/dirigent_desktop
mkdir -p "$DIR"

PREFIX=$DIR/prefix
APP_CMD="^($BIN|$PREFIX/lib/dirigent(-nightly)?/dirigent) --data-dir $DIR/data( |$)"
alive() {
  [ -f "$DIR/app.pid" ] && kill -0 "$(cat "$DIR/app.pid")" 2>/dev/null && return
  pgrep -f "$APP_CMD" >/dev/null
}
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
private_screen() {
  case $1 in */dirigent-screen-*/wayland-*) ;; *) return 1 ;; esac
  SWAYSOCK=${1%/*}/sway.sock swaymsg -t get_version >/dev/null 2>&1
}
screen_env() {
  WAYLAND_DISPLAY=$(cat "$DIR/display"); SWAYSOCK=${WAYLAND_DISPLAY%/*}/sway.sock
  export WAYLAND_DISPLAY SWAYSOCK
  unset DISPLAY
}
need_display() {
  [ -s "$DIR/display" ] || { echo "no session '$SESSION'; run: scripts/ui.sh start SCREEN" >&2; exit 1; }
  private_screen "$(cat "$DIR/display")" ||
    { echo "session '$SESSION': its screen $(cat "$DIR/display") is gone; open your private screen again and start" >&2; exit 1; }
  screen_env
}
need_app() {
  alive || { echo "the app of session '$SESSION' isn't running; run: scripts/ui.sh start SCREEN" >&2; exit 1; }
}
port() { cat "$DIR/port"; }
port_taken() { ss -ltnH "sport = :$1" | grep -q .; }
rpc() { node "$ROOT/scripts/ui-rpc.mjs" "$(port)" "$@"; }

build() {
  if ! "${NIX_BUILD[@]}" cargo build -q -p dirigent_desktop --message-format short 2>"$DIR/build.log"; then
    grep -E 'error' "$DIR/build.log" | grep -v '^error: could not compile' | head -30 || true
    echo "build failed, full output: $DIR/build.log" >&2
    exit 1
  fi
}

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

launch() {
  screen_env
  PORT=$(port 2>/dev/null || true)
  if [ -z "$PORT" ] || port_taken "$PORT"; then
    PORT=$((7800 + $(printf %s "$SESSION" | cksum | cut -d' ' -f1) % 190))
    while port_taken "$PORT"; do PORT=$((PORT + 1)); done
    echo "$PORT" >"$DIR/port"
  fi
  mkdir -p "$DIR/data" "$DIR/config"
  onboarding
  env -u DIRIGENT_URL -u DIRIGENT_DATA_DIR -u DIRIGENT_THREAD_ID -u UI_SESSION \
    VK_ICD_FILENAMES="$DIRIGENT_UI_VK_ICD" \
    DIRIGENT_PORT="$PORT" DIRIGENT_BIND=127.0.0.1 \
    DIRIGENT_PREFIX="$PREFIX" setsid "$BIN" --data-dir "$DIR/data" --config-dir "$DIR/config" >"$DIR/app.log" 2>&1 </dev/null &
  echo $! >"$DIR/app.pid"
  for _ in $(seq 1 200); do
    if swaymsg -r -t get_tree | grep -Ec '"app_id": ?"dirigent' >/dev/null; then sleep 1; return; fi
    sleep 0.1
  done
  echo "window never appeared, see $DIR/app.log" >&2; exit 1
}

case "${1:-}" in
  start)
    private_screen "${2:-}" || {
      echo "not a private screen: '${2:-}'. Open one with the computer tool (screen: private) and pass the WAYLAND_DISPLAY it names: scripts/ui.sh start /tmp/dirigent-screen-…/wayland-1" >&2
      exit 1
    }
    build
    kill_app
    echo "$2" >"$DIR/display"
    launch
    echo "session '$SESSION' ready on $WAYLAND_DISPLAY, service :$PORT, dir $DIR"
    ;;
  setup)
    # MUST KILL: rearchitect: set model and target over rpc, not by writing desktop.db kv across a restart
    need_display; need_app
    PROJECT=${2:-$DIR/scratch}
    case $PROJECT in /*) ;; *) PROJECT=$CALLER/$PROJECT ;; esac
    MODEL=${3:-claude-opus-5-5}
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
    LIMIT=${2:-300}; IDLE=0; SETTLE_POLLS=5; SECONDS=0
    [ "$LIMIT" -gt 0 ] 2>/dev/null || { echo "usage: scripts/ui.sh wait-idle [SECS]" >&2; exit 1; }
    while [ $IDLE -lt $SETTLE_POLLS ]; do
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
    unset DISPLAY WAYLAND_DISPLAY SWAYSOCK
    if [ -s "$DIR/display" ] && private_screen "$(cat "$DIR/display")"; then screen_env; fi
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
    sed -n '2,22p' "$0"
    exit 1
    ;;
esac
