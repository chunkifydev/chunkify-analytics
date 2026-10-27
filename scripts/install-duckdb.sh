#!/bin/sh
set -eu

# Match duckdb =1.20000.0 in Cargo.toml. Release archives include JSON and Parquet.
version=2.0.0
destination=${1:-target/lib}
architecture=${2:-$(uname -m)}
case "$(uname -s)/$architecture" in
    Linux/amd64|Linux/x86_64)
        platform=linux-amd64
        checksum=993ad89e7dfca66cd6da994c2cad090c2fc160ddf9f550498a098ab4bb13cfb1
        library=libduckdb.so
        ;;
    Linux/arm64|Linux/aarch64)
        platform=linux-arm64
        checksum=c60eecf91143ca6bbac55c4f4d40dc22520488b0ac257c86e0fe9a2b451cf2b7
        library=libduckdb.so
        ;;
    Darwin/arm64|Darwin/x86_64)
        platform=osx-universal
        checksum=14cd63180614cfd961f7959146ecd9087527ff20b67311efdc3fa1c29c18204d
        library=libduckdb.dylib
        ;;
    *)
        echo "Unsupported DuckDB platform: $(uname -s)/$architecture" >&2
        exit 1
        ;;
esac

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
archive="$temporary/duckdb.tar.gz"
curl --fail --location --silent --show-error --retry 3 \
    "https://github.com/duckdb/duckdb/releases/download/v$version/duckdb-shared-libs-$platform.tar.gz" \
    --output "$archive"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive")
else
    actual=$(shasum -a 256 "$archive")
fi
if [ "${actual%% *}" != "$checksum" ]; then
    echo "DuckDB archive checksum mismatch" >&2
    exit 1
fi
mkdir -p "$destination"
tar -xzf "$archive" -C "$destination"
test -f "$destination/$library"
