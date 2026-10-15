#!/bin/sh
# Installs the wt binary from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/nielsonrolim/wt/main/install.sh | sh
#
# WT_VERSION      release tag to install, e.g. v0.2.0 (default: the latest release)
# WT_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
set -eu

repo="https://github.com/nielsonrolim/wt"
dir="${WT_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	echo "wt install: $*" >&2
	exit 1
}

fetch() { # fetch URL FILE
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "needs curl or wget"
	fi
}

latest_tag() {
	# /releases/latest redirects to /releases/tag/<tag>.
	if command -v curl >/dev/null 2>&1; then
		url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repo/releases/latest")
	else
		url=$(wget -S --spider "$repo/releases/latest" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r')
	fi
	tag=${url##*/}
	case "$tag" in
	v*) echo "$tag" ;;
	*) die "could not find the latest release at $repo/releases" ;;
	esac
}

case "$(uname -s)/$(uname -m)" in
Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
Darwin/x86_64) target=x86_64-apple-darwin ;;
Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
*) die "no prebuilt binary for $(uname -s) $(uname -m); use: cargo install --git $repo --locked" ;;
esac

tag="${WT_VERSION:-$(latest_tag)}"
case "$tag" in
v*) ;;
*) tag="v$tag" ;;
esac
name="wt-$tag-$target.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading wt $tag for $target"
if ! fetch "$repo/releases/download/$tag/$name" "$tmp/$name" 2>/dev/null; then
	# Releases up to v0.2.0 shipped a glibc build instead of the static one.
	[ "$target" = x86_64-unknown-linux-musl ] || die "could not download $name"
	target=x86_64-unknown-linux-gnu
	name="wt-$tag-$target.tar.gz"
	echo "No static build in $tag; falling back to $target"
	fetch "$repo/releases/download/$tag/$name" "$tmp/$name" || die "could not download $name"
fi
fetch "$repo/releases/download/$tag/$name.sha256" "$tmp/$name.sha256" || die "could not download $name.sha256"

(
	cd "$tmp"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -c "$name.sha256" >/dev/null
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 -c "$name.sha256" >/dev/null
	else
		die "needs sha256sum or shasum to verify the download"
	fi
) || die "checksum mismatch for $name"

tar xzf "$tmp/$name" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/wt" "$dir/wt"
echo "Installed $dir/wt ($("$dir/wt" --version))"

case ":$PATH:" in
*":$dir:"*) ;;
*) echo "Note: $dir is not on your PATH. Add it to your shell profile:  export PATH=\"$dir:\$PATH\"" ;;
esac
