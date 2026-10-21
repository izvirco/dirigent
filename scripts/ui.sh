#!/usr/bin/env bash
# Drive a headless dirigent_desktop for UI tests. Guide: testing.md.
#
#   scripts/ui.sh start [W H]            build, (re)launch on a private virtual display (default 1100x720)
#   scripts/ui.sh shot [path] [X Y W H]  screenshot, optionally cropped (default <session>/shot.png)
#   scripts/ui.sh click X Y [right]      click at window px (screenshot px == window px)
#   scripts/ui.sh move X Y               hover
#   scripts/ui.sh drag X1 Y1 X2 Y2       press, move, release
#   scripts/ui.sh scroll X Y N           N notches down at X Y (negative: up)
#   scripts/ui.sh type "text"            type text
#   scripts/ui.sh key KEY...             key chords, xdotool names: Return Escape ctrl+n shift+Return
#   scripts/ui.sh log [N]                last N (40) lines of the app's stdout/stderr
#   scripts/ui.sh stop                   kill the app and its display
#   scripts/ui.sh reset                  stop and wipe the session's data (fresh service + desktop dbs)
#
# Each session (UI_SESSION, default "default") gets its own Xvfb display, service port and
# XDG_DATA_HOME under /tmp/dirigent-ui/<session>. Nothing touches the real desktop, mouse,
# keyboard, data or the service on :7777, and sessions can run in parallel.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# Tools (Xvfb, xdotool, imagemagick, lavapipe) and the app's runtime libs come from the dev
# shell. `nix develop` costs seconds per call, so the bits we need are cached until
# flake.nix / flake.lock change.
NIX=(nix develop --option warn-dirty false "$ROOT" -c)
if [ -z "${DIRIGENT_UI_VK_ICD:-}" ]; then
  ENV=/tmp/dirigent-ui/devenv-$(cat flake.nix flake.lock | sha256sum | cut -c1-16).sh
  if [ ! -s "$ENV" ]; then
    mkdir -p /tmp/dirigent-ui
    # shellcheck disable=SC2016
    "${NIX[@]}" bash -c 'printf "export PATH=%q LD_LIBRARY_PATH=%q DIRIGENT_UI_VK_ICD=%q\n" "$PATH" "$LD_LIBRARY_PATH" "$DIRIGENT_UI_VK_ICD"' >"$ENV.tmp"
    mv "$ENV.tmp" "$ENV"
  fi
  # shellcheck disable=SC1090
  source "$ENV"
  NIX_BUILD=("${NIX[@]}")
else
  NIX_BUILD=() # already inside the dev shell
fi

SESSION=${UI_SESSION:-default}
DIR=/tmp/dirigent-ui/$SESSION
BIN=$ROOT/target/debug/dirigent_desktop
mkdir -p "$DIR"

alive() { [ -f "$DIR/$1.pid" ] && kill -0 "$(cat "$DIR/$1.pid")" 2>/dev/null; }
# Both processes are started with setsid, so pid == process group: this also takes down
# the app's embedded service and pi runtime, and nothing outside the session.
kill_group() {
  if alive "$1"; then
    kill -TERM -- "-$(cat "$DIR/$1.pid")" 2>/dev/null || true
    for _ in $(seq 1 30); do alive "$1" || break; sleep 0.1; done
  fi
  rm -f "$DIR/$1.pid"
}
need_display() {
  alive xvfb || { echo "no running session '$SESSION'; run: scripts/ui.sh start" >&2; exit 1; }
  DISPLAY=:$(cat "$DIR/display"); export DISPLAY
}
settle() { sleep "${1:-0.15}"; }
free_port() {
  for p in $(seq 7800 7999); do
    ss -ltnH "sport = :$p" | grep -q . || { echo "$p"; return; }
  done
  echo "no free port in 7800-7999" >&2; exit 1
}

case "${1:-}" in
  start)
    W=${2:-1100}; H=${3:-720}
    "${NIX_BUILD[@]}" cargo build -q -p dirigent_desktop
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
    DISPLAY=:$(cat "$DIR/display"); export DISPLAY
    PORT=$(free_port)
    mkdir -p "$DIR/data"
    env -u WAYLAND_DISPLAY VK_ICD_FILENAMES="$DIRIGENT_UI_VK_ICD" XDG_DATA_HOME="$DIR/data" DIRIGENT_PORT="$PORT" \
      setsid "$BIN" >"$DIR/app.log" 2>&1 </dev/null &
    echo $! >"$DIR/app.pid"
    WIN=$(timeout 20 xdotool search --sync --onlyvisible --class dirigent | head -1) ||
      { echo "window never appeared, see $DIR/app.log" >&2; exit 1; }
    xdotool windowmove "$WIN" 0 0 windowsize "$WIN" "$W" "$H"
    settle 1
    echo "session '$SESSION' ready: ${W}x$H on $DISPLAY, service :$PORT, data $DIR/data, log $DIR/app.log"
    ;;
  shot)
    need_display
    OUT=${2:-$DIR/shot.png}
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
    sed -n '2,19p' "$0"
    exit 1
    ;;
esac
