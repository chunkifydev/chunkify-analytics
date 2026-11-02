# Repository instructions

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for
all commit messages and pull request titles, including squash-merge titles.

Format: `type(scope): description`. The scope is optional. Use an imperative,
concise description without a trailing period.

Choose the type that describes the change: `feat`, `fix`, `refactor`, `docs`,
`test`, `build`, `ci`, `perf`, `style`, or `chore`.

Examples:

- `feat(cli): add project subcommands`
- `fix(mcp): enforce project read permissions`
- `docs(repo): document commit conventions`

For a breaking interface change, add `!` before the colon and explain the
change in a `BREAKING CHANGE:` footer. Keep each commit focused on one logical
change.

Keep the README focused on installing and using the application. Put detailed
references and contributor instructions in `docs/`. Keep reusable tests and
benchmark tools in the repository, but write generated reports under ignored
`target/` directories. Do not commit progress logs, one-off experiment outputs,
or benchmark result files.

Persist generated API credentials only as hashes, including initial and default
project keys. Return raw keys once at creation or rotation, never in server logs
or credential files. When S3 owns the project registry, do not write a local
credential cache. Tests and examples must follow the same storage rules.
Do not add compatibility formats or migration paths when the user explicitly
excludes backward compatibility.

Keep multi-project analytics reads off the ingestion worker. Use independent
connections for concurrent reports so temporary project views cannot leak across
tenants. Bound active work per project and server, and test that a blocked tenant
does not prevent another tenant's reads or ingestion from completing.

Measure analytics read capacity with continuous ingestion and publication during
the timed workload, using the deployed storage lifecycle and resource limits.
Report offered and completed write rates, write failures and latency alongside
cold and cached reads. Seeding a dataset before reads does not establish mixed
read/write capacity.

Rank audience and page breakdowns and dimension-value lists by view count
descending before pagination. Rank error groups by affected views descending.
Use deterministic tie breakers and include the ranking position in cursors so
HTTP, MCP, exports and dashboard consumers share the same ordering.
