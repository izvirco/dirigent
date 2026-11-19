#!/bin/sh
# No macOS case: the system gives screen and accessibility access to a signed app, which a
# bare executable in our tarball isn't, so computer use there runs the user's own install.
set -eu

version=0.51.3
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    archive=linux-x86_64-binary.tar.gz
    sha256=044466f65db3976fa1ef302a3427ea12d5d4edd969263788d2fc16d793b95dde
    file=cua-driver
    ;;
  MINGW*-x86_64 | MSYS*-x86_64)
    archive=windows-x86_64-binary.zip
    sha256=3b6c81a8b4063b1eadea4cdc9a35481085e355ee1a6ffedb52903d354add1ae7
    file=cua-driver.exe
    ;;
  *)
    echo "no Cua Driver is shipped for $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac
dest=${1:?usage: packaging/cua-driver.sh DEST}
mkdir -p "$dest"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL -o "$tmp/$archive" "https://github.com/trycua/cua/releases/download/cua-driver-rs-v$version/cua-driver-rs-$version-$archive"
echo "$sha256  $tmp/$archive" | sha256sum -c >/dev/null
case $archive in
  *.zip) unzip -q "$tmp/$archive" "$file" -d "$tmp" ;;
  *) tar -xzf "$tmp/$archive" -C "$tmp" "./$file" 2>/dev/null || tar -xzf "$tmp/$archive" -C "$tmp" "$file" ;;
esac
install -m 755 "$tmp/$file" "$dest/$file"
curl -fsSL -o "$dest/cua-driver-LICENSE.md" "https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v$version/LICENSE.md"
echo "Cua Driver $version in $dest"
