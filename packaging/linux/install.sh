#!/bin/sh
# Installs Dirigent for this user, without root, from an unpacked release tarball
# (dirigent-<version>-linux-x86_64.tar.gz, built by .github/workflows/release.yml):
#
#   ~/.local/lib/dirigent/                        the tarball's dirigent/: the binary, runtime/, this script
#   ~/.local/bin/dirigent                         a link to the binary
#   ~/.local/share/applications/dirigent.desktop  the app menu entry, also offered for folders
#
# DIRIGENT_PREFIX puts all of it under another directory than ~/.local. Meant for tests
# (scripts/ui.sh): an update only installs there if the app has it in its environment too,
# else it moves the install to ~/.local.
#
# It is also how Dirigent updates: the app unpacks the next version's tarball, runs its
#   install.sh --update <pid> [app args...]
# and quits (dirigent_desktop/src/update.rs). This waits for <pid> to exit, installs, and
# starts Dirigent again with the app args (its --data-dir / --config-dir). So every update
# runs the new version's script, which can change in any release.
#
# Never change: the tarball's layout (this script at dirigent/install.sh), --update's arguments.
set -eu

src=$(cd "$(dirname "$0")" && pwd)
prefix=${DIRIGENT_PREFIX:-$HOME/.local}
dest=$prefix/lib/dirigent
apps=$prefix/share/applications

update=
if [ "${1:-}" = --update ]; then
  update=1
  pid=$2
  shift 2
  while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
  # Start Dirigent again however this goes: if installing fails, that's the old version,
  # which offers the update again.
  trap '"$dest/dirigent" "$@" >/dev/null 2>&1 </dev/null &' EXIT
fi

mkdir -p "$prefix/lib" "$prefix/bin" "$apps"
# Copied in next to the old install, then swapped for it: a failed copy leaves the old one
# as it was.
rm -rf "$dest.new" "$dest.old"
cp -R "$src" "$dest.new"
if [ -e "$dest" ]; then mv "$dest" "$dest.old"; fi
mv "$dest.new" "$dest"
rm -rf "$dest.old"
ln -sfn "$dest/dirigent" "$prefix/bin/dirigent"

# %f: a folder from the file manager's "Open with", which the app opens as a project.
cat >"$apps/dirigent.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Dirigent
Exec="$dest/dirigent" %f
Icon=$dest/dirigent.png
Terminal=false
Categories=Development;
MimeType=inode/directory;
StartupWMClass=dirigent
EOF
# The cache file managers look folder handlers up in.
update-desktop-database -q "$apps" 2>/dev/null || true

if [ -z "$update" ]; then
  echo "Installed Dirigent in $dest: start it from the app menu, or as $prefix/bin/dirigent."
  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "($prefix/bin isn't on your PATH: add it to start Dirigent as \`dirigent\`.)" ;;
  esac
fi
