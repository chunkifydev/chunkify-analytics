#!/bin/sh
set -eu

# DB-IP City Lite, CC BY 4.0. Update the month and checksum together.
version=2026-10
checksum=c839d407f85a1e0bddba354cef16b2c398182d1c34ed8d985555dce134b36b14
destination=${1:-target/share/chunkify-analytics}

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
archive="$temporary/city.mmdb.gz"
curl --fail --location --silent --show-error --retry 3 \
    "https://download.db-ip.com/free/dbip-city-lite-$version.mmdb.gz" \
    --output "$archive"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive")
else
    actual=$(shasum -a 256 "$archive")
fi
if [ "${actual%% *}" != "$checksum" ]; then
    echo "DB-IP City Lite checksum mismatch" >&2
    exit 1
fi
gzip -dc "$archive" > "$temporary/dbip-city-lite.mmdb"
mkdir -p "$destination"
install -m 644 "$temporary/dbip-city-lite.mmdb" "$destination/dbip-city-lite.mmdb"
