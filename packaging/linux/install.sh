#!/bin/sh
# Installs the directory this script is in, per user and without root (packaging/README.md):
#
#   install.sh                      the desktop, the command line and the service
#   install.sh --headless           the service alone, kept running: a remote host
#   install.sh --update PID ARGS    the desktop updating: once PID is gone, install, then start
#                                   the desktop again with ARGS
#   install.sh --update-service     the service updating itself: install over what is there, a
#                                   headless install staying one, and no more. The service
#                                   restarts itself.
set -eu

src=$(cd "$(dirname "$0")" && pwd)
prefix=${DIRIGENT_PREFIX:-$HOME/.local}
name=dirigent
title=Dirigent
service=dirigent_service
if [ -e "$src/nightly" ]; then
  name=dirigent-nightly
  title="Dirigent Nightly"
  service=dirigent_service-nightly
fi
dest=$prefix/lib/$name
apps=$prefix/share/applications
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$name.service
label=dev.sebba.$name
plist=$HOME/Library/LaunchAgents/$label.plist

mode=${1:-}
headless=
case $mode in
  "") ;;
  --headless) headless=1 ;;
  --update)
    pid=$2
    shift 2
    while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
    trap '"$dest/dirigent" "$@" >/dev/null 2>&1 </dev/null &' EXIT
    ;;
  --update-service)
    if [ ! -e "$dest/dirigent_desktop" ]; then headless=1; fi
    ;;
  *)
    echo "usage: install.sh [--headless]" >&2
    exit 2
    ;;
esac

mac=
if [ "$(uname -s)" = Darwin ]; then mac=1; fi

# The service starts through the user's login shell: what its agents run finds the programs
# the user's login profile puts on PATH, which the bare environment of systemd and launchd lacks.
start="exec '$dest/dirigent_service'"
shell=${SHELL:-/bin/sh}

write_unit() {
  mkdir -p "$(dirname "$unit")"
  cat >"$unit" <<EOF
[Unit]
Description=$title service

[Service]
ExecStart=$shell -l -c "$start"
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
}

write_plist() {
  mkdir -p "$(dirname "$plist")"
  cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$shell</string>
    <string>-l</string>
    <string>-c</string>
    <string>$start</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
</dict>
</plist>
EOF
}

autostart_outcome=
autostart() {
  if [ -n "$mac" ]; then
    write_plist
    domain=gui/$(id -u)
    launchctl bootout "$domain/$label" 2>/dev/null || true
    # A job just booted out may take a moment to be gone.
    if launchctl bootstrap "$domain" "$plist" 2>/dev/null || { sleep 2 && launchctl bootstrap "$domain" "$plist" 2>/dev/null; }; then
      autostart_outcome="It runs now, and whenever you log in to this Mac (launchd: $label)."
    else
      autostart_outcome="It starts when you log in to this Mac (launchd: $label)."
    fi
    return
  fi
  if ! command -v systemctl >/dev/null 2>&1; then return; fi
  if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR=/run/user/$(id -u)
    export XDG_RUNTIME_DIR
  fi
  # Lingering: the unit starts with the machine and outlives the login that installed it.
  linger=
  # Named: without a name loginctl takes the owner of the session, who under su or sudo is someone else.
  if loginctl enable-linger "$(id -un)" >/dev/null 2>&1; then linger=1; fi
  if ! systemctl --user show-environment >/dev/null 2>&1; then return; fi
  write_unit
  systemctl --user daemon-reload
  systemctl --user enable --quiet "$name.service"
  systemctl --user restart "$name.service"
  if [ -n "$linger" ]; then
    autostart_outcome="It runs now, and from when this machine starts (systemd: systemctl --user status $name)."
  else
    autostart_outcome="It runs now, and while you are logged in (systemd: systemctl --user status $name).
To keep it running after that, and from when this machine starts: sudo loginctl enable-linger $(id -un)"
  fi
}

mkdir -p "$prefix/lib" "$prefix/bin"
rm -rf "$dest.new" "$dest.old"
cp -R "$src" "$dest.new"
if [ -n "$headless" ]; then rm -f "$dest.new/dirigent_desktop" "$dest.new/dirigent" "$dest.new/dirigent.png"; fi
if [ -e "$dest" ]; then mv "$dest" "$dest.old"; fi
mv "$dest.new" "$dest"
rm -rf "$dest.old"
ln -sfn "$dest/dirigent_service" "$prefix/bin/$service"

if [ -n "$headless" ]; then
  rm -f "$prefix/bin/$name" "$apps/$name.desktop"
else
  ln -sfn "$dest/dirigent" "$prefix/bin/$name"
  if [ -z "$mac" ]; then
    mkdir -p "$apps"
    cat >"$apps/$name.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=$title
Exec="$dest/dirigent_desktop" %f
Icon=$dest/dirigent.png
Terminal=false
Categories=Development;
MimeType=inode/directory;
StartupWMClass=$name
EOF
    update-desktop-database -q "$apps" 2>/dev/null || true
  fi
fi

case $mode in
  --headless)
    autostart
    echo "Installed the $title service in $dest."
    if [ -n "$autostart_outcome" ]; then
      echo "$autostart_outcome"
    else
      echo "Nothing here keeps it running (no systemd user session): start it as $prefix/bin/$service."
    fi
    echo "To use this machine from a desktop, add it there in Settings › Hosts, as ssh://$(id -un)@$(uname -n)."
    ;;
  "")
    if [ -n "$mac" ]; then
      echo "Installed $title in $dest: start it from a terminal as $prefix/bin/$name."
    else
      echo "Installed $title in $dest: start it from the app menu, or as $prefix/bin/$name."
    fi
    case ":$PATH:" in
      *":$prefix/bin:"*) ;;
      *) echo "($prefix/bin isn't on your PATH: add it to start $title as \`$name\`.)" ;;
    esac
    ;;
esac
