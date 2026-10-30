# Continent, country and city

Every ingestion request uses a local MMDB lookup to determine the sender's
continent, country and city. Only `event_context.continent`,
`event_context.country`, and `event_context.city` enter storage. Country is an ISO 3166-1 alpha-2 code; city is an approximate English
name. Continent codes are `AF`, `AN`, `AS`, `EU`, `NA`, `OC`, and `SA`.
Missing results are null. There is no IP-storage setting or city toggle.

The Docker image and native release archive include
[DB-IP City Lite](https://db-ip.com/db/download/ip-to-city-lite). The database is
loaded into memory once at startup, about 121 MiB for the October 2026 edition.
No external lookup API, account, or credential is needed. Startup fails if the
database is missing, unreadable, or is not a City database.

## Installation and updates

Docker needs no extra configuration. For native releases, keep the archive's
`bin/`, `lib/`, and `share/` directories together. The executable finds the MMDB
at `../share/chunkify-analytics/dbip-city-lite.mmdb` relative to its own directory.
For source builds, run `sh scripts/install-geolocation.sh` once.

The installer pins a monthly edition and verifies its SHA-256 checksum. Updating
that pin and rebuilding distributes a newer edition. Running containers do not
download or refresh the database automatically.

To supply a newer database without rebuilding, mount a City MMDB read-only and
set `CHUNKIFY_GEOIP_DATABASE=/geolocation/city.mmdb`, or set
`server.geoip_database` in TOML. Relative TOML paths resolve beside the config
file. Restart the collector to load the new file. Updating the lookup database
does not rewrite locations already saved with events.

## Reverse proxies

Without a trusted proxy, the collector looks up its TCP peer. With trusted
proxies, it walks `X-Forwarded-For` from right to left and uses the first untrusted
address. Missing or invalid forwarding data results in unknown location.

Configure every proxy hop that appends to this header. For Cloudflare followed
by an AWS ALB, this includes both the ALB subnet ranges and Cloudflare's
[published IP ranges](https://www.cloudflare.com/ips/). Trusting only the ALB
would locate the Cloudflare edge. Other headers, including `CF-Connecting-IP`,
do not override the trust chain.

Loopback and private addresses have unknown continent, country and city. IP geolocation
can locate a VPN, mobile gateway, or proxy rather than the viewer. City names
are estimates, not precise positions or viewer identities.

## Reports

The API and MCP support `continent`, `country` and `city` as filters and as
`group_by` values for summary, quality and engagement. For example,
`GET /v1/summary?group_by=city&country=GB`, or MCP `get_metrics` with
`{"kind":"summary","group_by":"city","country":"GB"}`.
`GET /v1/dimensions/country` and MCP `list_dimension_values` list observed values
with view counts. The same queries are available as CSV exports.

Reports use the location of the first retained view start. Unknown locations
form a null group; an explicit location filter excludes them. City groups use
the stored name, so filter by country when comparing cities within one country.
Raw event filters use each event's own location. See [query details](api.md#filters-and-pagination).

## Data and attribution

This change stops collecting request IPs for new events. It does not erase old
events, backups, or logs belonging to your load balancer. Arbitrary properties
sent in event payloads remain unchanged; do not put IP addresses in them.

DB-IP City Lite is licensed under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Web applications must
link to DB-IP on pages displaying or using its results:

```html
<a href="https://db-ip.com">IP Geolocation by DB-IP</a>
```

The example dashboard includes this link in event details. Keep the
[database attribution](licenses/db-ip.txt) with redistributed packages. The
application's MIT license does not replace the database license.
