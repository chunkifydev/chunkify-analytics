#!/bin/sh
set -eu

# Install a pinned ClickHouse server binary for local development, tests and the
# native demo. Production installations can use any supported ClickHouse server.
# Usage: sh scripts/install-clickhouse.sh [DESTINATION] [ARCHITECTURE]
version=25.8.12.129
destination=${1:-target/clickhouse}
architecture=${2:-$(uname -m)}
base="https://github.com/ClickHouse/ClickHouse/releases/download/v$version-lts"
case "$(uname -s)/$architecture" in
    Linux/amd64|Linux/x86_64)
        asset=clickhouse-common-static-$version-amd64.tgz
        checksum=9fd023ffc2446ba00d817bd8cfadfbfd6d161b5710e0a8c4ab5e7a76f8a84b39
        ;;
    Linux/arm64|Linux/aarch64)
        asset=clickhouse-common-static-$version-arm64.tgz
        checksum=3365105838c502884359f18e3aa64f1b97cad002d8e2fe60aa7a4daa6d198b8e
        ;;
    Darwin/x86_64)
        asset=clickhouse-macos
        checksum=9a1df34b6ec2d60dfa699670f1bf3ae565e4da46f0b63610c39967a312143d7a
        ;;
    Darwin/arm64)
        asset=clickhouse-macos-aarch64
        checksum=541b149c0b73c44524cf3aff38541697736de266b36b6233811451a7922ee588
        ;;
    *)
        echo "Unsupported ClickHouse platform: $(uname -s)/$architecture" >&2
        exit 1
        ;;
esac

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
download="$temporary/$asset"
curl --fail --location --silent --show-error --retry 3 "$base/$asset" --output "$download"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$download")
else
    actual=$(shasum -a 256 "$download")
fi
if [ "${actual%% *}" != "$checksum" ]; then
    echo "ClickHouse download checksum mismatch" >&2
    exit 1
fi
mkdir -p "$destination"
case "$asset" in
    *.tgz)
        tar -xzf "$download" -C "$temporary"
        install -m 755 "$temporary/clickhouse-common-static-$version/usr/bin/clickhouse" "$destination/clickhouse"
        ;;
    *)
        install -m 755 "$download" "$destination/clickhouse"
        ;;
esac
"$destination/clickhouse" --version
