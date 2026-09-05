# Ozon MCP — product search, prices and reviews

[Русский](README.ru.md)

Search Ozon products, compare prices and specifications, and read customer reviews from your AI assistant. A local, read-only [Model Context Protocol](https://modelcontextprotocol.io/) server for shopping research on **Ozon.ru**.

The browser runs in the background by default. A persistent local profile keeps your session and region between runs, and structured results include warnings when part of a product page could not be retrieved.

## What you can do

- **Find products** by keyword, price range and sort order.
- **Inspect a product** by URL or SKU: prices, availability, seller, rating, images, specifications and description, where available.
- **Read reviews** with ratings, text, dates and available purchase/photo indicators.

For example, ask your assistant:

> Find wireless mice on Ozon under 4,000 ₽. Compare their specifications and summarize the reviews for the two most relevant options.

The server supplies the data; your assistant handles the comparison. It exposes three read-only tools and does not place orders or modify your cart. No Ozon Seller API key or paid scraping service is required.

## Designed for everyday use

- **No browser window during normal use.** Full Chromium runs in new headless mode. A visible window is an explicit option for signing in, never an automatic fallback.
- **A session that stays local.** Cookies and the selected region live in your browser profile; there is no hosted scraping backend.
- **Partial data is visible.** Missing values remain `null` where appropriate. Description and page-loading problems produce warnings rather than silently appearing complete.
- **Bounded requests.** Tools run sequentially, honour cancellation and timeouts, and return valid JSON or an explicit error.
- **Offline regression coverage.** Parser and browser-lifecycle tests use synthetic responses and a mock browser.

Ozon can still block requests or change its internal API. **Live Ozon access in the default headless mode has not yet been verified.** See [compatibility and known limitations](docs/behavior.md).

## Install

Requires **Node.js 20+** and Playwright's Chromium. A desktop session is only needed for optional manual sign-in.

```sh
git clone https://github.com/ibelyasov/ozon-mcp-server.git
cd ozon-mcp-server
npm ci
npx playwright install chromium
```

Add the following to an MCP client that supports **stdio**, replacing the absolute paths with your own. Clients with a different configuration format can use the same command, arguments and environment variables.

```json
{
  "mcpServers": {
    "ozon": {
      "command": "node",
      "args": ["/absolute/path/ozon-mcp-server/src/index.js"],
      "env": {
        "OZON_HEADLESS": "true",
        "OZON_USER_DATA_DIR": "/absolute/path/ozon-browser-profile"
      }
    }
  }
}
```

If your client cannot find `node`, use its absolute executable path. Restart or reconnect the MCP after changing its configuration.

### Optional sign-in

1. Set `OZON_HEADLESS=false`, reconnect the MCP and make a tool request to open Chromium.
2. Sign in and check your region in that window.
3. Stop the MCP, set `OZON_HEADLESS=true`, and reconnect using the same profile path.

Keep the profile outside the repository. It contains your session; do not publish it or share it between simultaneous MCP processes. Whether Ozon accepts that session in headless mode still depends on its current checks.

## Tools

| Tool | Inputs | Result |
|---|---|---|
| `ozon_search` | `query`; optional `sort`, `priceMin`, `priceMax`, `limit` | Up to 36 product results with prices and links |
| `ozon_product_details` | `product`: Ozon URL, SKU or product slug ending in a SKU | Product details and available description |
| `ozon_product_reviews` | `product`; optional `limit` | Up to 30 reviews per call |

Search sort values: `popular`, `price`, `price_desc`, `rating`, `new`, `discount`. Prices are in RUB; the selected region, account and payment conditions can affect them.

[Configuration, warnings and resource limits →](docs/behavior.md)

## Development

```sh
npm test
```

Tests run locally without opening Chromium, contacting Ozon or reading an account profile. They cover parsing, input validation, cancellation, retries and context cleanup; they do not establish live marketplace availability.

## Credits and license

Based on [Pir0manT/ozon-mcp-server](https://github.com/Pir0manT/ozon-mcp-server) and the original [eduard256/ozon-mcp-server](https://github.com/eduard256/ozon-mcp-server). This fork continues that work with background operation, parser corrections and request-lifecycle tests. Upstream Git history and author attribution are retained.

MIT is declared in the upstream project metadata. This project is unofficial and is not affiliated with Ozon.
