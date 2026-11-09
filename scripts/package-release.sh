#!/bin/sh
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "Usage: sh scripts/package-release.sh VERSION TARGET [BINARY]" >&2
    exit 1
fi
version=$1
target=$2
case "$version" in
    ''|*[!0-9A-Za-z.-]*)
        echo "Invalid release version: $version" >&2
        exit 1
        ;;
esac
case "$target" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *)
        echo "Unsupported release target: $target" >&2
        exit 1
        ;;
esac

cd "$(dirname "$0")/.."
binary=${3:-target/$target/release/chunkify-analytics}
name="chunkify-analytics-$version-$target"
mkdir -p target/release-dist
temporary=$(mktemp -d target/.release-pack.XXXXXX)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
bundle="$temporary/$name"
mkdir -p "$bundle/bin" "$bundle/share/chunkify-analytics"
install -m 755 "$binary" "$bundle/bin/chunkify-analytics"
install -m 644 target/share/chunkify-analytics/dbip-city-lite.mmdb "$bundle/share/chunkify-analytics/"
cp README.md LICENSE config.toml.example openapi.json "$bundle/"
cp -R docs "$bundle/docs"
tar -czf "target/release-dist/$name.tar.gz" -C "$temporary" "$name"
echo "Release archive: target/release-dist/$name.tar.gz"
