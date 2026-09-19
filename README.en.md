# Ozon MCP — thorough product research

[Русский](README.md)

Ozon MCP is a local [Model Context Protocol](https://modelcontextprotocol.io/) server for researching products on Ozon.ru. Version 2.0.2 is written in Rust with `rmcp` 3.2 and native `agent-browser` 0.36.0. It does not modify carts, orders, accounts, or regions.

Status: **development**. The live source implements variants, review photos, and city observation from an exact first-cell field; verification returned Moscow. A public probe found no safe link for other seller offers, so offers remain `unsupported`. Full readiness still requires the remaining live gates and target MCP client acceptance.

## Tools

- `ozon_get_context` observes the shared context, region, and capabilities;
- `ozon_search` searches and refines results;
- `ozon_get_products` reads product cards in batches;
- `ozon_get_reviews` reads reviews and observed aggregates;
- `ozon_get_images` returns validated images through stored references;
- `ozon_list_research` and `ozon_get_research` read the local journal;
- `ozon_append_research_note` appends an idempotent agent note.

An Ozon Card price remains a separate observed payment condition and is never replaced with a regular price. Unknown and incomplete data is returned as `unknown`, `partial`, or a typed error. References and cursors are bound to one `researchId` and `contextId`; navigation cursors expire after 30 minutes.

An observed city cannot guarantee that two accounts in the same city are distinguishable; such an account switch may remain invisible to the server. Local product-section continuations read a bounded cached snapshot with its original timestamp.

Marketplace calls only read Ozon, but persist local events and evidence. Journal retention is 30 days with a 512 MiB cap and at least ten minutes of idle protection for leased research. Notes are local writes: reusing an `operationId` with identical content returns the prior result; changed content returns `CONFLICT`.

## Install and run

You need Rust 1.95 and **exactly** `agent-browser` 0.36.0.

```sh
git clone https://github.com/ibelyasov/ozon-mcp-server.git
cd ozon-mcp-server
cargo build --release --locked
cargo install agent-browser --version 0.36.0 --locked
agent-browser install
```

The normal process is a stdio frontend. Multiple frontends share a private local broker, one persistent browser profile, and one observed context.

```json
{"mcpServers":{"ozon":{"command":"/absolute/path/ozon-mcp-server/target/release/ozon-mcp-server","env":{"OZON_AGENT_BROWSER_BIN":"/absolute/path/to/agent-browser","OZON_HEADLESS":"true","OZON_DATA_DIR":"/absolute/private/path/ozon-mcp"}}}}
```

`OZON_DATA_DIR` contains private IPC and journal state and must be owned by the current user with mode `0700`. `OZON_USER_DATA_DIR` selects the single persistent profile. `OZON_BROKER_SOCKET` may override the socket within the data directory. The Unix socket path must fit within 103 bytes. macOS and Linux are supported; Windows is not.

Set `OZON_HEADLESS=false` before broker startup for manual account/region setup; the window opens after an MCP context call. Changing a frontend environment does not reconfigure an existing broker. Stop that broker gracefully (Ctrl+C for foreground `--broker`, or SIGTERM), or allow its ten-minute inactivity shutdown, before switching mode. There is no automatic sign-in, region change, or fallback profile. Keep the profile and data directory outside the repository.

## Agent research workflow

The agent records mandatory and preferred requirements and a budget, tries different queries, categories, and observed refinements, then examines finalists, reviews, and photos until further passes stop improving the choice. Before concluding, it refreshes price and delivery, recommends one primary option and at most two alternatives, and explains rejected competitors, coverage, and uncertainty. A dynamic catalog cannot be claimed as exhaustively searched.

See [docs/behavior.md](docs/behavior.md) for details and [contracts/](contracts/README.md) for machine-readable contracts.

## Development

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 test --locked
cargo +1.95.0 clippy --locked --all-targets -- -D warnings
npm ci && npm run check:page && npm test
UV_CACHE_DIR=/tmp/ozon-contracts-uv uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

`rust/page.ts` compiles to tracked `rust/page.js`; commit both together. Offline checks do not prove live Ozon availability.

## Authors and license

This project is based on [Pir0manT/ozon-mcp-server](https://github.com/Pir0manT/ozon-mcp-server) and the original [eduard256/ozon-mcp-server](https://github.com/eduard256/ozon-mcp-server). Git history and attribution are preserved. The source projects declare MIT metadata. This is an unofficial project and is not affiliated with Ozon.

Image DNS compatibility and `OZON_IMAGE_DOH_FALLBACK=auto|off` are documented in [server behavior](docs/behavior.md#image-dns-compatibility).

The optimized local build passed 30 MCP calls across six search categories, product/review image transport, simultaneous clients, and broker crash recovery. See [validation and remaining limits](docs/behavior.md#validation-on-2026-09-13).
