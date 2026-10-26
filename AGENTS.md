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
