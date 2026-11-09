#!/bin/sh
set -eu

# Install a pinned ClickHouse server binary for local development, tests and the
# native demo. Production installations can use any supported ClickHouse server.
# Usage: sh scripts/install-clickhouse.sh [DESTINATION] [ARCHITECTURE]
version=26.10.2.103
destination=${1:-target/clickhouse}
architecture=${2:-$(uname -m)}
base="https://github.com/ClickHouse/ClickHouse/releases/download/v$version-stable"
case "$(uname -s)/$architecture" in
    Linux/amd64|Linux/x86_64)
        asset=clickhouse-common-static-$version-amd64.tgz
        checksum=ec811478449184dea496ba17c3f76fdd3aeb2013dd5041676bd00041fed98d17
        ;;
    Linux/arm64|Linux/aarch64)
        asset=clickhouse-common-static-$version-arm64.tgz
        checksum=bd59e072630c8884037575c59dad664330fd69c4247da334c9cf1b3f79a1a4c4
        ;;
    Darwin/x86_64)
        asset=clickhouse-macos
        checksum=8bdb041c299a849ebd9bbba72ffca925a24482abec690ae698be63818eece1ab
        ;;
    Darwin/arm64)
        asset=clickhouse-macos-aarch64
        checksum=7f43a3c4d5f41e42ae6ec275c1531ca44b6018caae15217394fd85181ca1df59
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
