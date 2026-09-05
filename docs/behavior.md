# Configuration and behavior

[Обзор на русском](../README.md) · [English overview](../README.en.md)

This page describes the Rust 0.4.0 server built with `rmcp` and `agent-browser` 0.36.0.

## Runtime configuration

| Variable | Default | Behavior |
|---|---|---|
| `OZON_AGENT_BROWSER_BIN` | `agent-browser` from `PATH` | Optional absolute path to the pinned native driver. The executable must report exactly `agent-browser 0.36.0`; another version stops server startup. |
| `OZON_BROWSER_EXECUTABLE` | Unset | Optional path to an existing full Chrome executable. If unset, agent-browser uses the Chrome installed by `agent-browser install`. |
| `OZON_HEADLESS` | `true` | Only the case-insensitive value `false` enables a visible browser. There is no automatic visible fallback. |
| `OZON_USER_DATA_DIR` | `~/.ozon-mcp-rust-profile` | Persistent profile for cookies, storage and region. It is exclusively locked by one MCP process. |
| `OZON_CITY` | Unset | Selects a city through current Ozon UI selectors. Failure returns `REGION_SELECTION_FAILED`; successful selector actions still require manual verification of the saved region. |
| `OZON_HIDE_WINDOW` | Ignored | Deprecated. Use `OZON_HEADLESS`; the server only emits a diagnostic when this variable is present. |

The browser starts on the first tool call. Agent-browser receives a ten-minute idle timeout, and the server replaces a session before reuse after roughly that interval. `OZON_HEADLESS=false` is an explicit manual session; errors never open a window automatically.

The driver runs from a private temporary directory with an empty private config, an empty plugin list, WebMCP disabled and a cleared environment containing only a small system allowlist. Ambient agent-browser plugins, providers and user configuration are not loaded. Neither the Rust server nor the native driver needs a Node.js runtime.

## Data path and results

The server first fetches Ozon's internal `composer-api` inside the Ozon tab. When that request returns HTTP 403 or 307, it may navigate to the exact requested Ozon page and extract an allowlist of public product widgets. A different origin, product, review page or search is rejected. Profile, address and order widgets are excluded from returned data.

For `BROWSER_COMMAND_FAILED` only, a read-only request gets one retry in a confirmed fresh browser session, still inside the 55-second tool deadline. HTTP errors and challenges are returned directly; there is no HTTP retry loop.

Successful tools return the same JSON object as MCP `structuredContent` and text content. The server never truncates JSON: oversized results fail explicitly. It can only return fields present in the received page; unknown values remain `null` where defined.

Product details request the secondary description page whenever the base page has no nonempty text, including when the base contains description images only. The merged result prefers nonempty base text, otherwise uses secondary text, and returns the deduplicated union of images from both pages.

| Warning/error | Meaning |
|---|---|
| `DESCRIPTION_FETCH_FAILED` | The secondary description request failed; available base fields remain, together with `DESCRIPTION_TEXT_EMPTY` or `DESCRIPTION_EMPTY` when applicable. |
| `DESCRIPTION_TEXT_EMPTY` | Description images are available after merging, but description text is absent. |
| `DESCRIPTION_EMPTY` | Both description text and images are absent after merging. |
| `SEARCH_WIDGET_MISSING` | The expected search block is absent; an empty result is not proof of no matching products. |
| `PRODUCT_WIDGETS_MISSING` | Expected product heading or price blocks are absent. |
| `REVIEWS_WIDGET_MISSING` | The expected reviews block is absent. |
| `REGION_SELECTION_FAILED` | `OZON_CITY` was set but could not be applied through Ozon's current interface. |
| `CAPTCHA_OR_BLOCKED` | Ozon supplied neither an accepted response nor allowed public widgets. |
| `RESPONSE_TOO_LARGE` / `RESULT_TOO_LARGE` | The page response or final tool result exceeded its limit; partial JSON is not returned. |
| `SERVER_BUSY` | Eight requests are already admitted, including the active request. |
| `TOOL_TIMEOUT` | The 55-second whole-tool deadline, including queue time, expired. |
| `BROWSER_CLEANUP_FAILED` | The server could not confirm that its private browser closed and refuses further browser work. |

Unrecovered HTTP errors and browser command failures are tool errors. Raw page responses and unrestricted driver stderr are not returned to the MCP client.

## Concurrency, timeouts and cancellation

- Up to eight calls are admitted per server process, including the active call; a single browser mutex permits one actual browser operation at a time.
- The whole tool deadline is 55 seconds including queue time.
- Normal agent-browser commands have a 45-second deadline; page evaluation commands have 40 seconds.
- The in-page Ozon fetch aborts after 35 seconds.
- An accepted Ozon page response is limited to 4 MiB. Driver JSON is separately bounded, and the final result is limited to 60,000 UTF-16 code units.
- Agent-browser receives a ten-minute idle timeout.

On cancellation, failure or timeout, the server closes only the private browser whose local CDP endpoint it captured and validated. It sends the standard CDP `Browser.close` command to that loopback endpoint, then asks agent-browser to close the session. This narrow interrupt path is not a general CDP implementation or a second browser driver. If shutdown cannot be confirmed, the session is poisoned and subsequent work fails closed instead of sharing an uncertain browser.

Browser startup is cancellation-shielded only while the server acquires ownership: opening the private browser is bounded to 15 seconds and capturing its validated local CDP endpoint to another 5 seconds. Client cancellation is reported immediately, while the admitted queue slot and browser lock remain owned until this bounded acquisition and cleanup finish. This prevents a detached browser from being orphaned before the server knows which endpoint it may close.

## Compatibility and verification

As of September 5, 2026:

- Rust 0.4.0 server startup and MCP tool listing passed.
- All nine ordinary Rust offline tests passed. They cover parsers and local contract boundaries and have no Ozon dependency.
- The ignored-by-default real-Chromium test `cancelled_evaluation_closes_private_browser_and_can_restart` passed in 8.00 seconds. It cancelled endless JavaScript, closed the captured private browser and successfully launched and used a replacement, without contacting Ozon.
- The release 0.4.0 stdio server completed a successful live headless run on macOS with all three advertised tools and exit code 0 in 25.78 seconds. Search returned two items in 19.82 seconds, including Logitech MX Master 3S SKU `947750106` at 6,859 RUB. Details for that SKU returned rating 4.9 but no description text in 3.86 seconds. A later description check confirmed two images and no text, with `DESCRIPTION_TEXT_EMPTY`, in 3.88 seconds. Reviews returned two entries out of 2,618 in 1.90 seconds; their aggregate rating was unavailable (`null`).
- Earlier runs received HTTP 403 responses and one transient `BROWSER_COMMAND_FAILED`. Those results remain historical evidence that Ozon access can change; the successful run does not guarantee future availability.
- Windows and Linux compatibility for this fork remains unverified.

Ozon may change response formats and anti-bot checks at any time. These checks do not establish uninterrupted access, completeness across product categories, or acceptance of a signed-in profile in headless mode. Verify the region and product page before relying on prices or availability.

Run the real-browser cancellation test only with a disposable profile and explicit pinned executables:

```sh
OZON_USER_DATA_DIR=/absolute/path/to/disposable-test-profile \
OZON_AGENT_BROWSER_BIN=/absolute/path/to/agent-browser \
OZON_BROWSER_EXECUTABLE=/absolute/path/to/full-chrome \
cargo test --locked cancelled_evaluation_closes_private_browser_and_can_restart -- --ignored
```

## Troubleshooting

- **Driver version error:** install exactly agent-browser 0.36.0 or point `OZON_AGENT_BROWSER_BIN` at that binary.
- **Chrome missing:** run `agent-browser install`, or set `OZON_BROWSER_EXECUTABLE` to an existing full Chrome executable.
- **Browser unexpectedly opens:** remove `OZON_HEADLESS=false` and restart old MCP processes. No error path enables visible mode.
- **Profile is locked:** stop the other MCP process that owns the same profile. Do not delete an active profile to bypass its lock.
- **Region selection fails:** start an explicit visible session, select the region manually, then reuse that profile in headless mode.
- **HTTP 403 or blocked page:** the server reports the failure. A visible login may help but is not a guaranteed workaround.

Do not include cookies, account profiles or raw account responses in bug reports. Use synthetic examples or remove personal and session data before sharing a reproducer.
