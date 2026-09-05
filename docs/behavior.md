# Configuration and behavior

[Обзор на русском](../README.md) · [English overview](../README.en.md)

## Browser settings

| Variable | Default | Behavior |
|---|---|---|
| `OZON_HEADLESS` | `true` | Full Chromium with `channel: "chromium"`, without a window. Only `false` enables a visible browser; no automatic headed fallback. |
| `OZON_USER_DATA_DIR` | `~/.ozon-mcp-userdata` | Persistent profile for cookies, storage and selected region. One MCP process per profile. |
| `OZON_CITY` | Unset | Best-effort region selection through the Ozon interface. Verify manually if the region matters; selectors can change. |
| `OZON_HIDE_WINDOW` | Ignored | Deprecated. Moving a window off-screen did not guarantee that it would leave application focus alone. Use `OZON_HEADLESS`. |

The browser starts on the first request and closes after ten minutes without active requests. Cancellation closes the affected context. A retry may reopen it, always in the configured mode. The persistent profile remains on disk.

## Results and warnings

The server reads Ozon's internal `composer-api`. It can only return fields present in the received response. Brand parsing requires explicit evidence; an unknown brand is `null`. Unknown purchase/photo indicators are also nullable, so missing evidence is not reported as `false`.

| Warning/error | Meaning |
|---|---|
| `DESCRIPTION_FETCH_FAILED` | The additional description request failed; other available product fields are returned. |
| `DESCRIPTION_EMPTY` | No description was extracted from the successful response. |
| `SEARCH_WIDGET_MISSING` | The expected search-results block is absent. An empty result is not necessarily “no matching products.” |
| `PRODUCT_WIDGETS_MISSING` | Expected product heading or price blocks are absent. |
| `REVIEWS_WIDGET_MISSING` | The expected reviews block is absent. |
| `RESULT_TOO_LARGE` | The tool result exceeds the output limit; no truncated JSON is returned. |

HTTP errors and challenge failures are reported as tool errors. No automatic visible browser is opened to resolve them.

## Resource limits

- One tool at a time per server process.
- 55 seconds per tool, including queue time.
- 45 seconds per browser fetch; 4 MiB per composer response.
- 60,000 characters per tool result; errors are separately bounded.

A timeout cancels active browser work. The queue retains ownership until the operation settles, so another tool cannot start using a context still being closed.

## Compatibility and verification

As of 2026-09-05:

- **Version 0.3.1, headed mode:** MCP startup, product search, two product pages and review retrieval were exercised on macOS Apple Silicon. Description requests were intermittently blocked; a captured response confirmed successful description parsing.
- **Version 0.3.2, default headless mode:** reviewed statically and covered by 31 passing offline tests with synthetic responses and a mock browser. No live Ozon or Chromium test was performed for this mode.
- Windows and Linux compatibility has not been verified for this fork.

These checks do not establish uninterrupted access, completeness across all product categories, or acceptance of a signed-in session in headless mode. Ozon may change its response format or anti-bot checks. Prices and availability can vary by region, session and payment conditions; consult the product page before buying.

## Troubleshooting

- **Browser unexpectedly opens:** check for `OZON_HEADLESS=false` and restart old MCP processes after updating. `OZON_HIDE_WINDOW` no longer controls visibility.
- **Profile is locked:** stop other MCP/browser processes using that profile. Do not delete an active profile to resolve the lock.
- **Blocked or challenge error:** the server reports the failure. A manual visible login may help, but is not a guaranteed fix.
- **Descriptions are missing:** inspect `warnings`; a network failure and an empty parsed description are reported separately.

Do not include cookies, account profiles or raw account responses in bug reports. Use synthetic examples or remove personal/session data before sharing a reproducible case.
