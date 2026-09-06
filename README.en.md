# Ozon MCP — product search, prices and reviews

[Русский](README.md)

A local, read-only [Model Context Protocol](https://modelcontextprotocol.io/) server that gives an AI assistant three tools for searching **Ozon.ru**, reading product details and reading reviews. Version 0.4.0 is written in Rust with `rmcp` and native `agent-browser` 0.36.0. The server and driver do not require Node.js.

The browser runs without a window by default. A local profile keeps cookies and the selected region between runs; the server cannot place orders or modify a cart, and it needs neither an Ozon Seller API key nor a paid scraping service.

> **Current 0.4.0 status (September 5, 2026):** the release Rust build passed a live headless run on macOS. MCP started with all three tools: search returned two products in 19.82 seconds; details returned a price of 6,859 RUB and a 4.9 rating for SKU `947750106` in 3.86 seconds; reviews returned two entries out of 2,618 in 1.90 seconds. A subsequent description check confirmed two images without separate description text. The full stdio session exited successfully in 25.78 seconds. Earlier HTTP 403 responses and a transient driver error remain historical limitations: this successful run does not guarantee future Ozon availability.

## Capabilities

- `ozon_search` searches by query, price and sort, returning available Ozon facets and a continuation cursor. Up to 36 products per call include ratings, review counts, price conditions and available delivery labels.
- `ozon_product_details` reads available prices, seller, rating, images, specifications and description by URL, SKU or slug.
- `ozon_product_reviews` reads up to 30 available reviews.

Follow returned facet `searchUrl` links to refine a search and `nextCursor` to continue it. Available filters depend on the category and Ozon response; incomplete coverage and unknown conditions remain explicit. Use details and reviews to verify shortlisted products. See [search parameters and limitations](docs/behavior.md#search-refinement-and-continuation).

Each successful call returns the same valid JSON as MCP `structuredContent` and text content. Unknown values remain `null`, and missing expected widgets produce warnings. Prices and availability depend on region, session and payment conditions.

## Install 0.4.0

You need Rust 1.88+ and **exactly** `agent-browser` 0.36.0. The server rejects any other driver version at startup.

```sh
git clone https://github.com/ibelyasov/ozon-mcp-server.git
cd ozon-mcp-server
cargo build --release --locked
```

Install the native `agent-browser` 0.36.0 binary from its [official release page](https://github.com/vercel-labs/agent-browser/releases/tag/v0.36.0), or build it with Cargo:

```sh
cargo install agent-browser --version 0.36.0 --locked
```

Then install agent-browser's managed Chrome separately:

```sh
agent-browser install
```

To use an existing full Chrome installation instead, set `OZON_BROWSER_EXECUTABLE` to its existing executable.

## Configure an MCP client

Use the absolute path to the built stdio server. An absolute path to the pinned driver is recommended, especially when the MCP client starts with a restricted `PATH`.

```json
{
  "mcpServers": {
    "ozon": {
      "command": "/absolute/path/ozon-mcp-server/target/release/ozon-mcp-server",
      "env": {
        "OZON_AGENT_BROWSER_BIN": "/absolute/path/to/agent-browser",
        "OZON_HEADLESS": "true"
      }
    }
  }
}
```

The default profile is `~/.ozon-mcp-rust-profile`. Restart or reconnect the MCP after changing its configuration.

### Visible manual session

For manual sign-in or reliable region selection, set `OZON_HEADLESS=false`, reconnect the MCP, and call a tool. Check the account and region in the opened window, then stop the MCP and restore `OZON_HEADLESS=true` while keeping the same profile. Visible mode is never enabled automatically.

`OZON_CITY` attempts to select a city through Ozon's interface. If it cannot apply the requested city, the request fails with `REGION_SELECTION_FAILED`; the server does not present the saved profile region as the requested one. Even successful selector actions do not prove which region Ozon saved, so manual verification in the persistent profile remains the reliable option.

Keep the profile outside the repository because it may contain an account session. Only one MCP process may own a profile at a time.

## Development

Edit the browser script in `rust/page.ts`; TypeScript 7.0.2 compiles it into the tracked `rust/page.js`. Rust embeds the generated JavaScript, so Cargo builds and server execution do not require Node.js. Do not edit `page.js` by hand.

To change the script, use Node.js 22+ with npm:

```sh
npm ci
npm run build:page
npm run check:page
npm test
```

`check:page` checks types and verifies that `page.js` matches its source. Commit both files together; CI repeats this check and the browser script's offline tests.

```sh
cargo test --locked
```

After the search upgrade, all 29 ordinary Rust tests and 10 browser-script tests passed, covering parsing, facets, continuation, URL boundaries and public metadata extraction. A separate ignored-by-default real-Chromium test cancels endless JavaScript, closes the private browser and successfully restarts it; it passed in 8.00 seconds. Neither test path depends on Ozon or establishes marketplace availability. See [configuration and behavior](docs/behavior.md) for the opt-in command, isolation and resource limits.

## Credits and license

Based on [Pir0manT/ozon-mcp-server](https://github.com/Pir0manT/ozon-mcp-server) and the original [eduard256/ozon-mcp-server](https://github.com/eduard256/ozon-mcp-server). Git history and author attribution are retained. MIT is declared in the upstream project metadata. This project is unofficial and is not affiliated with Ozon.
