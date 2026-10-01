# CodeGraph

CodeGraph is an additional source-navigation tool for this repository. The CLI
and Codex MCP server are installed globally; each checkout has its own local
`.codegraph/` index. The current local installation is version 1.6.1.

## Setup and index maintenance

From the root of a new checkout or worktree:

```sh
codegraph init
codegraph status --json
```

If initialization offers a fallback because watching is unavailable, choose
manual sync to keep the existing Git hooks unchanged.

The existing checkout is already initialized. To reconcile changes manually:

```sh
codegraph sync
codegraph status --json
```

The MCP daemon watches source changes and reconciles missed changes when it
starts. After changing indexing filters or upgrading extraction behavior, rebuild
with `codegraph index`. A complete status with zero pending changes confirms that
the index is current. Initialize each worktree separately to keep branch sources
separate.

The root `.gitignore` excludes `.codegraph/`, `.work/`, build output, dependency
directories, and local profiles. CodeGraph honors Git ignore rules and its own
built-in dependency/cache exclusions. Default language detection and filtering
are sufficient here; no `codegraph.json` overrides or Git sync hooks are needed.

## Codex integration

The global `codegraph` MCP entry runs `codegraph serve --mcp` and exposes the
default `codegraph_explore` tool. Verify it with `codex mcp get codegraph --json`.
Its environment sets `CODEGRAPH_TELEMETRY=0`, `DO_NOT_TRACK=1`, and
`CODEGRAPH_NO_UPDATE_CHECK=1`; the CLI's saved telemetry setting is also disabled.
Open a new Codex session after adding or changing the MCP entry. Project-local
MCP configuration is unnecessary with this global registration.

## Navigation

Use focused queries with symbol or file names:

```sh
codegraph explore "commit_reviews commit_operation prepare_reply"
codegraph callers prepare_reply --json
codegraph callees commit_reviews --json --limit 100
```

Treat reported relationships as navigation hints. Inspect critical dependencies
in source before deciding impact; missing edges do not prove absence. CodeGraph
does not reliably classify inline Rust tests, so choose checks using
[verification](../behavior.md#verification). The detailed local evaluation and
A/B measurements are in [the research note](../research/codegraph.md).
