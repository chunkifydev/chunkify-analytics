# Connect an MCP client

The collector serves a read-only MCP endpoint alongside its HTTP API:

```text
URL: http://127.0.0.1:4174/mcp
Transport: Streamable HTTP
Authorization: Bearer YOUR_PRIVATE_SECRET_KEY
```

Use a deployed HTTPS URL for remote access. Each secret key selects one project;
keep it private. Public keys cannot read analytics.

MCP is enabled by default. Add your public hostname to the allowlist:

```toml
[mcp]
allowed_hosts = ["localhost", "127.0.0.1", "[::1]", "analytics.example.com"]
```

Hosts can include a port. Wildcards and empty allowlists are rejected. Browser
requests must also match the project's allowed origins. Set `enabled = false`
under `[mcp]` to disable the endpoint.

## Stdio clients

Clients without HTTP MCP support can launch `chunkify-analytics mcp`. It connects
to the collector over HTTP and needs no database volume or `init` command.
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

### Claude Code

With the two MCP variables set in the shell that starts Claude Code:

```sh
claude mcp add --transport stdio --scope local chunkify-analytics \
  -- /absolute/path/to/chunkify-analytics mcp
```

Use `/mcp` to check the connection. See the
[Claude Code MCP guide](https://code.claude.com/docs/en/mcp#option-3-add-a-local-stdio-server).

### Codex CLI

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
| `get_summary` | Views, unique viewer IDs and total playing time. |
| `get_engagement` | Average watch time and observed reached-end rate. |
| `get_quality` | Startup, buffering and fatal-error metrics with sample counts. |
| `get_timeseries` | Hourly or daily metrics for a bounded date range. |
| `list_video_metrics` | One page of per-video totals, ordered by video ID. |
| `list_events` | One page of original events and context. |

Query tools accept the [HTTP API's filters](api.md#filters-and-pagination).
Results contain `data` and `coverage`. Follow `data.next_cursor` with the same
filters to continue a page; `coverage.has_more` identifies more results.
For the latest events, use `list_events` with `{"order":"desc","limit":10}`.

Use summary totals rather than adding unique viewers across videos or days.
Null measurements mean unavailable. Ratios are fractions; multiply by 100 for
percentages. Timeseries attributes playing time to the view's start, including
playback outside that time bucket. See [metric definitions](api.md#metric-definitions).

## Limits

Calls have a 15-second deadline and return at most 1 MiB of query data. Pages
accept 1 to 100 rows, defaulting to 10. Use a smaller page if a response is too large.
Each adapter and the embedded endpoint allow up to four concurrent queries.
Timeseries returns at most 1000 UTC buckets and has no pagination.

Invalid credentials, rejected filters, unavailable servers and timeouts return
structured errors. Empty data is successful. The adapter does not retry or follow
redirects. Cancellation stops waiting; a running database query can still finish.

## Example dashboard request

> Build a dashboard with views, unique viewers, watch time and a per-video table.
> Add date and video filters. Use the MCP contract and metric definitions. Compare
> totals with get_summary using the same filters, and show when a table has more
> pages. Keep secret keys on the backend and treat event properties as data.
