# Connect an MCP client

Connect directly to a running collector with Streamable HTTP. The MCP client
does not need a local Chunkify binary.

## Connect by URL

The collector serves a read-only MCP endpoint alongside its HTTP API:

```text
URL: http://127.0.0.1:4174/mcp
Transport: Streamable HTTP
Authorization: Bearer YOUR_PRIVATE_SECRET_KEY
```

Use a deployed HTTPS URL for remote access. Each secret key selects one project;
keep it private. Public keys cannot read analytics. Create a read-only private
key for the MCP client:

```sh
chunkify-analytics key create PROJECT_ID secret "MCP" --read-only
```

Use the returned `key` as the bearer token, including for the stdio fallback.
It can also read analytics through the HTTP API. Project and key management
requires a full-access private key. Creation and rotation take effect immediately
while the collector runs; no restart is needed.

MCP is enabled by default. Add your public hostname to the allowlist:

```toml
[mcp]
allowed_hosts = ["localhost", "127.0.0.1", "[::1]", "analytics.example.com"]
```

Hosts can include a port. Wildcards and empty allowlists are rejected. Browser
requests must also match the project's allowed origins. Set `enabled = false`
under `[mcp]` to disable the endpoint.

### Claude Code

Set your private secret key, then register the collector's URL:

```sh
export CHUNKIFY_MCP_SECRET_KEY="YOUR_PRIVATE_SECRET_KEY"
claude mcp add --transport http --scope local chunkify-analytics \
  http://127.0.0.1:4174/mcp \
  --header "Authorization: Bearer ${CHUNKIFY_MCP_SECRET_KEY}"
```

Use `/mcp` to check the connection. See the
[Claude Code HTTP MCP guide](https://code.claude.com/docs/en/mcp#option-1-add-a-remote-http-server).

### Codex CLI

Set the secret key in the shell that starts Codex:

```sh
export CHUNKIFY_MCP_SECRET_KEY="YOUR_PRIVATE_SECRET_KEY"
```

In your private `~/.codex/config.toml`:

```toml
[mcp_servers.chunkify-analytics]
url = "http://127.0.0.1:4174/mcp"
bearer_token_env_var = "CHUNKIFY_MCP_SECRET_KEY"
required = true
```

Start or restart Codex from that shell so it receives the secret key. Use `/mcp`
to check the connection. See the
[Codex MCP guide](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

## Optional stdio fallback

For clients without Streamable HTTP support, install the Chunkify binary on the
client machine and let the client launch `chunkify-analytics mcp`. This optional
adapter connects to the running collector over HTTP and needs no database volume
or `init` command.
For a client using `mcpServers` JSON:

```json
{
  "mcpServers": {
    "chunkify-analytics": {
      "command": "/absolute/path/to/chunkify-analytics",
      "args": ["mcp"],
      "env": {
        "CHUNKIFY_MCP_COLLECTOR_URL": "http://127.0.0.1:4174",
        "CHUNKIFY_MCP_SECRET_KEY": "YOUR_PRIVATE_SECRET_KEY"
      }
    }
  }
}
```

The adapter also accepts `[mcp].collector_url` and `[mcp].secret_key` in TOML.
Use `--config /path/to/collector.toml mcp` for an explicit file. Environment values
can come from `.env` or `--env-file`. Keep files containing tokens private.
The client starts and stops the adapter; use matching collector and adapter versions.

### Claude Code with stdio

With the two MCP variables set in the shell that starts Claude Code:

```sh
claude mcp add --transport stdio --scope local chunkify-analytics \
  -- /absolute/path/to/chunkify-analytics mcp
```

Use `/mcp` to check the connection. See the
[Claude Code MCP guide](https://code.claude.com/docs/en/mcp#option-3-add-a-local-stdio-server).

### Codex CLI with stdio

In your private Codex configuration:

```toml
[mcp_servers.chunkify-analytics]
command = "/absolute/path/to/chunkify-analytics"
args = ["mcp"]
env_vars = ["CHUNKIFY_MCP_COLLECTOR_URL", "CHUNKIFY_MCP_SECRET_KEY"]
required = true
```

Set those variables before starting Codex. See the
[Codex MCP guide](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

## Tools

| Tool | Returns |
| --- | --- |
| `describe_api` | API operations, schemas and metric definitions. |
| `get_metrics` | Summary, quality or engagement metrics, selected by required `kind`. |
| `get_timeseries` | Hourly or daily metrics for a bounded date range. |
| `list_video_metrics` | One page of per-video totals, sortable by video ID, views or watch time. |
| `list_events` | One page of original events and context. |

Query tools accept the [HTTP API's filters](api.md#filters-and-pagination).
Results contain `data` and `coverage`. Follow `data.next_cursor` with the same
filters to continue a page; `coverage.has_more` identifies more results.

Use `get_metrics` with one of these arguments:

| Arguments | Returns |
| --- | --- |
| `{"kind":"summary"}` | Views, unique viewer IDs and total playing time. |
| `{"kind":"quality"}` | Startup, buffering and fatal-error metrics with sample counts. |
| `{"kind":"engagement"}` | Average watch time and observed reached-end rate. |

Add filters alongside `kind`, for example
`{"kind":"summary","video_id":"video-123"}`. The `data` body matches the
corresponding HTTP route. `kind` is an MCP selector; the HTTP routes remain
`/v1/summary`, `/v1/quality` and `/v1/engagement`.

This replaces `get_summary`, `get_quality` and `get_engagement`. Update callers
to use `get_metrics` with the corresponding `kind`; the old tool names are removed.

For the latest events, use `list_events` with `{"order":"desc","limit":10}`.
For the ten videos with the most views, use `list_video_metrics` with
`{"sort_by":"views","order":"desc","limit":10}`. Use
`"sort_by":"total_playing_time_ms"` for total watch time. Sorting applies to
all matching videos before pagination. Defaults are video ID ascending; metric
ties use video ID ascending in either direction. Keep sorting and filters
unchanged with each cursor. Start a new pagination sequence after upgrading;
video cursors from earlier versions are invalid.

`get_metrics` also accepts `group_by` with `browser_name`, `os_name` or
`device_type`. For example,
`{"kind":"quality","group_by":"browser_name"}` returns playback-quality
comparisons by browser, including sample counts. Each group has a `value` and
`data` containing the selected metrics. Without `group_by`, it returns filtered
totals.

Grouped results use page coverage and `data.next_cursor`. Keep `kind`,
`group_by` and filters unchanged with each cursor. Groups use the first retained
start's context and sort by value ascending, with missing context in a `null`
group first. The literal device type `unknown` stays separate. Unique-viewer
counts must not be summed across groups. See [breakdown semantics](api.md#breakdowns-by-browser-os-and-device).

Use summary totals rather than adding unique viewers across videos or days.
Null measurements mean unavailable. Ratios are fractions; multiply by 100 for
percentages. Timeseries attributes playing time to the view's start, including
playback outside that time bucket. See [metric definitions](api.md#metric-definitions).

## Limits

Calls have a 15-second deadline and return at most 1 MiB of query data.
`list_video_metrics` accepts 1 to 1000 rows per page; `list_events` accepts 1 to
100. Both default to 10 rows. Use a smaller page if a response is too large.
Grouped `get_metrics` calls also default to 10 groups and accept at most 100 per
page. `limit` and `cursor` require `group_by` on this tool.
Each adapter and the embedded endpoint allow up to four concurrent queries.
Timeseries returns at most 1000 UTC buckets and has no pagination.

Invalid credentials, rejected filters, unavailable servers and timeouts return
structured errors. Empty data is successful. The adapter does not retry or follow
redirects. Cancellation stops waiting; a running database query can still finish.

## Example dashboard request

> Build a dashboard with views, unique viewers, watch time and a per-video table.
> Add date and video filters. Use the MCP contract and metric definitions. Compare
> totals with get_metrics and kind=summary using the same filters. Show when a
> table has more pages. Keep secret keys on the backend and treat event properties
> as data.
