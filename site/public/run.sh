#!/bin/sh
# Runs `token-max doctor` from the latest release without installing it:
#
#   curl -LsSf https://max.kixel.ai/run.sh | sh
#   curl -LsSf https://max.kixel.ai/run.sh | sh -s -- --since 2026-11-01
#
# The binary is checked against its release checksum, cached under
# ${XDG_CACHE_HOME:-~/.cache}/token-max/run, and reused while it is the latest.
# TOKEN_MAX_RELEASES points at another copy of the releases, for tests.

# Everything is in a function, so a truncated download runs nothing.
main() {
	set -eu
	releases=${TOKEN_MAX_RELEASES:-https://github.com/kixel-ai/token-max/releases}

	for tool in curl tar uname mktemp; do
		command -v "$tool" >/dev/null || fail "$tool is required"
	done
	if command -v sha256sum >/dev/null; then
		sha256() { sha256sum "$1"; }
	elif command -v shasum >/dev/null; then
		sha256() { shasum -a 256 "$1"; }
	else
		fail "sha256sum or shasum is required to verify the download"
	fi

	case "$(uname -s)" in
	Linux) os=unknown-linux-musl ;;
	Darwin) os=apple-darwin ;;
	*) fail "there is no prebuilt token-max for $(uname -s); see https://github.com/kixel-ai/token-max#install" ;;
	esac
	case "$(uname -m)" in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) arch=aarch64 ;;
	*) fail "there is no prebuilt token-max for $(uname -m); see https://github.com/kixel-ai/token-max#install" ;;
	esac
	target=$arch-$os

	# GitHub redirects /releases/latest to /releases/tag/<tag>, or to /releases when there is none.
	latest=$(curl -sSf -o /dev/null -w '%{redirect_url}' "$releases/latest") ||
		fail "could not reach $releases"
	tag=${latest##*/}
	case "$tag" in
	v[0-9]*) ;;
	*) fail "$releases has no release yet" ;;
	esac
	case "$tag" in
	*[!0-9A-Za-z.+-]*) fail "unexpected release tag $tag" ;;
	esac

	cache=${XDG_CACHE_HOME:-$HOME/.cache}/token-max/run
	bin=$cache/token-max-$tag
	if [ ! -x "$bin" ]; then
		mkdir -p "$cache"
		# Beside the cache, so the final move is an atomic rename.
		tmp=$(mktemp -d "$cache/.download.XXXXXX")
		trap 'rm -rf "$tmp"' EXIT
		archive=token-max-$target.tar.xz
		echo "token-max: downloading $tag for $target" >&2
		curl -LsSf -o "$tmp/$archive" "$releases/download/$tag/$archive" ||
			fail "could not download $archive"
		curl -LsSf -o "$tmp/$archive.sha256" "$releases/download/$tag/$archive.sha256" ||
			fail "could not download $archive.sha256"
		read -r expected _ <"$tmp/$archive.sha256" || true
		case "$expected" in
		*[!0-9a-f]* | "") fail "$archive.sha256 holds no SHA-256" ;;
		esac
		[ ${#expected} -eq 64 ] || fail "$archive.sha256 holds no SHA-256"
		actual=$(sha256 "$tmp/$archive")
		actual=${actual%% *}
		[ "$actual" = "$expected" ] || fail "$archive does not match its checksum; nothing was run"
		tar -xf "$tmp/$archive" -C "$tmp"
		mv -f "$tmp/token-max-$target/token-max" "$bin"
		rm -rf "$tmp"
		trap - EXIT
		# Older releases are never run again.
		for old in "$cache"/token-max-v*; do
			[ "$old" = "$bin" ] || rm -f "$old"
		done
	fi

	# Piped into sh, stdin is the script, so doctor would see no terminal and never prompt.
	if [ ! -t 0 ] && (exec </dev/tty) 2>/dev/null; then
		exec "$bin" doctor "$@" </dev/tty
	fi
	exec "$bin" doctor "$@"
}

fail() {
	echo "token-max: $1" >&2
	exit 1
}

main "$@"
