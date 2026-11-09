#!/bin/sh
set -eu

# Start a disposable ClickHouse server on 127.0.0.1:8123 for development and
# tests, and wait until it answers. Data and logs stay in DIRECTORY.
# Usage: sh scripts/start-clickhouse.sh [DIRECTORY] [BINARY]
directory=${1:-target/clickhouse/data}
binary=${2:-target/clickhouse/clickhouse}
# Another server on the port would answer the readiness check below while this
# one fails to bind, and tests would then use the wrong server.
if curl --fail --silent --max-time 2 http://127.0.0.1:8123/ping > /dev/null 2>&1; then
    echo "A server already answers on 127.0.0.1:8123; stop it first" >&2
    exit 1
fi
mkdir -p "$directory"
directory=$(cd "$directory" && pwd)
(cd "$directory" && nohup "$binary" server -- --path="$directory/" \
    --listen_host=127.0.0.1 --http_port=8123 --tcp_port=9000 \
    --mysql_port=0 --postgresql_port=0 > "$directory/server.log" 2>&1 &)
attempt=0
until curl --fail --silent --show-error --max-time 2 http://127.0.0.1:8123/ping > /dev/null 2>&1; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 60 ]; then
        echo "ClickHouse did not start; see $directory/server.log" >&2
        exit 1
    fi
    sleep 1
done
echo "ClickHouse is listening on http://127.0.0.1:8123"
