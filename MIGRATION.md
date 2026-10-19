# Repository extraction

On 2026-10-19, the collector was extracted from `chunkifydev/chunkify-player`
commit `390a0bd` into the private `chunkifydev/chunkify-analytics` repository.
`git-filter-repo` retained the relevant history and authors. Commit hashes change
when unrelated files are removed; `history-map.txt` maps the original collector
commits to their extracted hashes.

| Original path | Standalone path |
| --- | --- |
| `services/analytics-collector/` | Repository root |
| `tests/collector/` | `tests/http/` |
| `.github/workflows/collector.yml` | Same path |

The Rust crate, executable name, API routes and database format are unchanged by
this extraction. MCP stays in the collector executable. The player and the entire
client SDK, including its adapters and enrichment changes, stay in
`chunkify-player`. Integration tests use the published `@chunkify/analytics@0.4.0`
package from npm, pinned in `package-lock.json`.

Existing benchmark reports are historical evidence. Their recorded source paths,
commit hashes, measurements and hashes were preserved. Use `history-map.txt` to
locate the extracted commits. Historical investigation documents may show their
original monorepo commands; use the current README and npm scripts for new runs.

The extraction includes tracked source and test artifacts only. Local databases,
keys and build caches are not repository content. Existing local collector data
stays at its original location; deployment/data-directory moves are a separate
operation. Keep `COLLECTOR_DATA_DIR` pointed at the intended persistent directory.
The governing specification remains issue #381 in `chunkifydev/chunkify`.

## Verification

Verification results will be recorded here before the initial private push.
