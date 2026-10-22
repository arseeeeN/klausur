#!/bin/sh
# Install or upgrade klausur on Linux or macOS.
#
#   curl -fsSL https://github.com/arseeeeN/klausur/releases/latest/download/install.sh | sh
#
# Environment:
#   KLAUSUR_BIN_DIR   where to put the binary (default ~/.local/bin)
#   KLAUSUR_REPO      GitHub owner/name to install from
set -eu

repo="${KLAUSUR_REPO:-arseeeeN/klausur}"
bin_dir="${KLAUSUR_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "klausur: no build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

download() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else echo "klausur: curl or wget is required" >&2; exit 1; fi
}

asset="klausur-$target"
base="https://github.com/$repo/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $asset ..."
download "$base/$asset" "$tmp/$asset"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS"

expected="$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
if command -v sha256sum >/dev/null 2>&1; then actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"; fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "klausur: checksum mismatch for $asset; not installing" >&2; exit 1
fi

mkdir -p "$bin_dir"
chmod 755 "$tmp/$asset"
mv -f "$tmp/$asset" "$bin_dir/klausur"
echo "Installed $("$bin_dir/klausur" --version) to $bin_dir/klausur"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "Note: $bin_dir is not on your PATH." ;;
esac
