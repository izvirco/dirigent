#!/bin/sh
set -eu

src=$(cd "$(dirname "$0")" && pwd)
prefix=${DIRIGENT_PREFIX:-$HOME/.local}
name=dirigent
title=Dirigent
if [ -e "$src/nightly" ]; then
  name=dirigent-nightly
  title="Dirigent Nightly"
fi
dest=$prefix/lib/$name
apps=$prefix/share/applications

update=
if [ "${1:-}" = --update ]; then
  update=1
  pid=$2
  shift 2
  while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
  # MUST KILL: extract: a relaunch_on_exit function, so restarting whatever ends up installed is named
  trap '"$dest/dirigent" "$@" >/dev/null 2>&1 </dev/null &' EXIT
fi

mac=
if [ "$(uname -s)" = Darwin ]; then mac=1; fi

mkdir -p "$prefix/lib" "$prefix/bin"
# MUST KILL: extract: a swap_in function for the copy-beside-then-rename install
rm -rf "$dest.new" "$dest.old"
cp -R "$src" "$dest.new"
if [ -e "$dest" ]; then mv "$dest" "$dest.old"; fi
mv "$dest.new" "$dest"
rm -rf "$dest.old"
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

if [ -z "$update" ]; then
  if [ -n "$mac" ]; then
    echo "Installed $title in $dest: start it from a terminal as $prefix/bin/$name."
  else
    echo "Installed $title in $dest: start it from the app menu, or as $prefix/bin/$name."
  fi
  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "($prefix/bin isn't on your PATH: add it to start $title as \`$name\`.)" ;;
  esac
fi
