#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
manifest="$root/packaging/qualification-manifest.toml"

host="$(uname -s)-$(uname -m)"
case "$host" in
    Darwin-arm64) platform=osx-arm64 ;;
    Darwin-x86_64) platform=osx-amd64 ;;
    Linux-x86_64) platform=linux-amd64 ;;
    *) printf 'No pinned rclone archive for %s\n' "$host" >&2; exit 1 ;;
esac

# Read the simple section and quoted-string fields in the checked-in manifest.
read_pin() {
    local value
    value=$(awk -F '"' -v section="[$1]" -v key="$2 = " '
        /^\[/ { selected = ($0 == section) }
        selected && $1 == key { print $2; exit }
    ' "$manifest")
    if [[ -z "$value" ]]; then
        printf 'Missing %s.%s in %s\n' "$1" "$2" "$manifest" >&2
        return 1
    fi
    printf '%s\n' "$value"
}

verify_sha256() {
    local result actual
    result=$(shasum -a 256 "$1")
    actual=${result%% *}
    if [[ "$actual" != "$2" ]]; then
        printf 'SHA-256 mismatch for %s: expected %s, got %s\n' "$1" "$2" "$actual" >&2
        return 1
    fi
}

version=$(read_pin rclone version)
url=$(read_pin "rclone.$platform" url)
archive_sha256=$(read_pin "rclone.$platform" archive_sha256)
binary_sha256=$(read_pin "rclone.$platform" binary_sha256)
destination="$root/.tools/rclone/$version"
binary="$destination/rclone"

if [[ -e "$binary" ]]; then
    verify_sha256 "$binary" "$binary_sha256"
    if [[ ! -x "$binary" ]]; then
        printf 'Pinned rclone is not executable: %s\n' "$binary" >&2
        exit 1
    fi
    printf 'Pinned rclone %s already verified at %s\n' "$version" "$binary"
    exit 0
fi

mkdir -p "$destination"
scratch=$(mktemp -d "$destination/.fetch.XXXXXXXX")
trap 'rm -f "$scratch/archive.zip" "$scratch/rclone"; rmdir "$scratch"' EXIT
archive=${url##*/}
curl --fail --location --silent --show-error "$url" --output "$scratch/archive.zip"
verify_sha256 "$scratch/archive.zip" "$archive_sha256"
unzip -p "$scratch/archive.zip" "${archive%.zip}/rclone" > "$scratch/rclone"
verify_sha256 "$scratch/rclone" "$binary_sha256"
chmod 755 "$scratch/rclone"
mv -f "$scratch/rclone" "$binary"
printf 'Installed pinned rclone %s at %s\n' "$version" "$binary"
