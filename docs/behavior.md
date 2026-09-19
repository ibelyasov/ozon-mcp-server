# Configuration and behavior

[Русский обзор](../README.md) · [English overview](../README.en.md) · [Contracts](../contracts/README.md)

This describes version 2.0.2, built with Rust 1.95, `rmcp` 3.2, and native `agent-browser` 0.36.0.

## Process and configuration

Each MCP client starts a stdio frontend. Frontends connect through private local IPC to one broker, which owns one persistent browser profile and one observed account/region context. Another frontend never creates a fallback profile.

| Variable | Default | Meaning |
|---|---|---|
| `OZON_DATA_DIR` | Platform user-data location | Private socket, journal, and state. It must belong to the current OS user, must not be a symlink, and has mode `0700`. |
| `OZON_USER_DATA_DIR` | Under `OZON_DATA_DIR` | The one persistent browser profile. |
| `OZON_BROKER_SOCKET` | Under `OZON_DATA_DIR` | Optional Unix socket override, directly inside the data directory. Its encoded path is at most 103 bytes. |
| `OZON_AGENT_BROWSER_BIN` | `agent-browser` from `PATH` | Driver executable, exactly version 0.36.0. |
| `OZON_BROWSER_EXECUTABLE` | Unset | Optional existing Chrome executable. |
| `OZON_HEADLESS` | `true` | Only `false` requests a visible browser. |
| `OZON_MCP_TEXT_MODE` | `compact` | Successful calls put the complete result in `structuredContent` and a short pointer in text. Set `json` only for legacy text-only clients that require the complete JSON duplicated in text. Any other value fails startup. |

The broker and IPC support macOS and Linux. Windows is unsupported. The server never automatically signs in or changes region. To configure either, set `OZON_HEADLESS=false` before broker startup and request context from an MCP client, then make the change manually in the visible window. A running broker retains its launch configuration; stop it gracefully before changing mode.

The profile can contain cookies. The journal excludes credentials, cookies, account identifiers, full addresses, raw DOM/widgets, and unrestricted stderr. Keep both directories outside the repository.

## Context, observations, and references

`ozon_get_context` reports observed access, account, region, and capability state. `contextId` identifies the broker generation shared by agents. For an anonymous header, the source reads the supported city cell. After sign-in, that cell can contain delivery timing and the next cell a pickup address; neither is treated as a city.

If the authenticated header exposes the exact supported same-origin `/modal/addressbook` link, the broker reads that page through ordinary GET navigation. The page script extracts only the city prefix from the single selected saved address. The full address and address-book widgets remain inside the browser. The broker restores the home page, rechecks the authenticated state, and accepts the city only after consistent observations. Missing or ambiguous data, unexpected redirects, or inconsistent observations stay unverified or fail the call. Final-route validation happens inside the page; the observed numeric `__rr` retry parameter is accepted without exporting its value. Region evidence points to the fixed address-book page separately from header account/access evidence. No address is selected or saved by the server. Cancellation never starts another browser merely to restore the home page.

The verified basis is the observed city of the selected saved address; it is not a delivery guarantee. Switching between accounts in the same city may be unobservable, so `contextId` does not guarantee account identity.

Opaque `researchId`, `productRef`, `imageRef`, and `evidenceRef` values are authority only within private IPC. References bind to their research and context. Navigation and section cursors also bind filters and projection and expire after 30 minutes. Context changes invalidate navigation rather than mixing observations.

Search, product, review, and image calls only read Ozon and write bounded local journal records. Results preserve observation time, context, source association, and evidence paths. Unknown fields stay unknown; a missing or malformed source section is not a confirmed empty list.

## Response views and repeated searches

`ozon_search`, `ozon_get_products`, and `ozon_get_reviews` accept `view: compact | comparison | full`; `compact` is the default. Every view retains requested sections, statuses, unknown values, context, observation time, warnings, and row-level `evidenceRefs`. `compact` and `comparison` set `evidenceDetail: journal` and omit inline evidence metadata; resolve selected references locally with `ozon_get_research` using `section: evidence` and up to 20 `evidenceRefs`. `full` sets `evidenceDetail: inline` and is the compatibility view for consumers that expect all inline fields. A field omitted by a presentation view was not requested in that representation; it must not be interpreted as an observed `unknown` value.

Compact search rows omit URL, seller, delivery, and image metadata, except when a delta row must carry one of those fields because it changed. Comparison search rows retain URL, seller, and delivery metadata. Product calls default to `include: ["characteristics"]` in compact and comparison views, and to characteristics plus offers in full view. An explicit `include` list is honored in every view, including unsupported section statuses. Compact review presentation removes only redundant identity and empty optional metadata; review text is not shortened.

Search also accepts `repeatMode: full | delta`, defaulting to `full`. Both modes preserve every source row. Each row carries `novelty.status` (`new`, `unchanged`, or `changed`), `previousProductRef`, and `changedFields`. In delta mode, a previously observed row uses `representation: delta` and `baselineProductRef`; it always retains identity, prices, availability, range-match assessment, provenance, and every changed field. Retrieve the immutable earlier candidate locally with `ozon_get_research`, `section: candidates`, and one to 20 `productRefs`. Candidate snapshots retain their captured Context and observation time, so a baseline is historical evidence rather than current catalog state.

Search refinements are excluded by default in compact and comparison views. A fresh full search includes them by default; a continuation does not. `includeFacets` explicitly overrides those defaults. Refinements are returned in cached pages controlled by `refinementLimit` (default 12, maximum 100), each capped at 8,000 UTF-16 units. Follow `start.refinementsCursor` to read another cached page without an Ozon or browser request. Source truncation is reported separately from local refinement paging. Item cursors bind `view` and `repeatMode`; refinement cursors additionally bind `refinementLimit`, and incompatible overrides are rejected.

Review facets are opt-in through `includeFacets`; a fresh full-view request defaults them on. Continuations inherit the captured setting; an incompatible explicit override is rejected.

Successful MCP responses always carry the complete result once in `structuredContent`. In the default `OZON_MCP_TEXT_MODE=compact`, text contains only a short pointer to that structured result. `json` preserves compatibility with text-only clients by duplicating the complete JSON in text. Typed error responses are unchanged by this setting. Image calls keep text at content index 0 and append image content after it.

Prices use integer minor units. `ozon_card` is a distinct observed payment condition and is never replaced with `regular`. A visible price does not prove account eligibility. Delivery labels remain as displayed and are not guarantees; an unknown delivery fee is not zero.

A requested search price range is sent to Ozon as a source refinement, whose price basis is unknown. It does not guarantee that returned displayed prices, or Ozon Card prices, lie within the range. Range responses expose `priceRangeAssessment` with comparison basis `displayed_search_price` and `sourceFilterGuaranteesMatch: false`; each item has `matchesDisplayedPriceRange` (`true`, `false`, or `null` when comparison is unavailable). Out-of-range results keep their source order and carry an explicit warning. Agents must inspect these fields when enforcing a budget.

Search refinements represent complete observed states. Agents follow returned references instead of constructing category-specific URLs. Pagination is bounded and dynamic. `hasNext: false` requires a confirmed end; local limits produce partial or unknown coverage.

Product customs duty is returned separately as `customsDuty` with `status`, integer `amountMinor`, currency, source label and evidence refs. When either description or duty is missing from the base page, the gateway reads at most one existing secondary product fragment. This may add one read for products with a base description, under the same batch deadline. The amount is only available when observed in a supported customs widget; otherwise it is `unknown`/`null` with `CUSTOMS_DUTY_UNKNOWN`. A missing charge is never zero. Product prices and duty do not establish shipping fees or a checkout total.

Characteristics from `webShortCharacteristics` are a short summary: nonempty results are `partial`, with `hasNext: null` when no cached continuation remains. `truncated: false` only means no local size truncation; it does not establish source completeness. Empty unverified characteristics are `unknown`. The extractor currently has no verified full-specification source. A standalone rich-description placeholder `Заголовок` is treated as missing descriptive text. Generic marketplace titles are preserved rather than rewritten from agent guesses.

Search preserves marketplace ordering and source titles. It can return accessories or mismatched configurations; agents must verify product type and required attributes. A price condition stays `unknown` unless explicit source evidence identifies it, and a typed price in a later card observation does not retrospectively prove the search price condition.

Product batches preserve selector order and per-item errors. They observe context before the batch and confirm it once after live retrieval, reserving up to five seconds of the existing operation budget for confirmation. Each live product retains its retrieval timestamp. A changed or unconfirmed context rejects all live results in that batch. The bounded serial batch can still time out; an error distinguishes an operation that ran out of time from one that could not start within the queue budget. Retry only failed selectors in smaller batches. Characteristics retain source labels and values. Variants are implemented from observed SKU mappings. A public live probe found no safe other-offers link, so seller offers remain explicitly `unsupported`. Product-section continuations use a bounded cached snapshot and preserve its original observation timestamp. Empty or whitespace-only description text is `unknown`, with `text: null` and `hasNext: null`; it is not proof that the product has no description. Observed rich-description text is extracted from public text/title blocks. Review dates are UTC RFC3339 timestamps. `reviews.coverage.returned` counts the current response; `uniqueSeen` counts distinct review identities returned across the current cursor chain, including cached continuations. Replaying a cursor does not grow that count, and a fresh query/refinement starts a new chain. Stable source review IDs are deduplicated; rows without a source ID remain distinct even when their text matches. Duplicate source IDs are omitted from subsequent responses. Coverage state uses immutable identity chunks with periodic bounded checkpoints and is limited to 30,000 identities per chain; reconstruction checks cancellation and its deadline. Each cursor preserves its own continuation state for replay and branching, and the journal quota still applies. Repeated source pages without new reviews stop with `SOURCE_CHANGED` after three consecutive no-progress pages, or immediately for a same-path loop. Older review cursors that lack cumulative state fail with `INVALID_REFERENCE`; start a fresh review request. Reviews preserve aggregation scope; review photos are implemented through research-bound image refs without inventing purchase filters.

Images accept only stored refs associated with the same research. Fetches enforce HTTPS, allowed origin, redirect and destination checks, raster MIME and decode, one MiB per image, four MiB per call, and a 20-megapixel decoded-source limit. `image_content` becomes `available` after a successful image response is verified in the current observable context and returns to `unverified` when that context changes. Each returned image has a content index and hash; one failure does not hide successful siblings.

## Journal and errors

Research events, evidence, product references, and notes survive broker restart. Summaries include at most 100 product refs; every candidate remains enumerable through events. Page caps are 25 events, 20 evidence entries, and 10 notes.

Retention is 30 days with a 512 MiB logical database plus WAL budget. A request lease protects active research for at least ten idle minutes; no research is permanently active. Maintenance removes oldest idle research as a whole. If protected records prevent recovery, writes fail with `STORAGE_FULL`. A marketplace result cannot succeed if its journal write fails.

`ozon_append_research_note` atomically validates refs against the same research. `(researchId, operationId)` is idempotent for canonical identical JSON; changed reuse returns `CONFLICT`. Source text and notes are untrusted data, never instructions.

Whole-tool failures use a stable typed code and omit success `structuredContent`. Batch tools may preserve successful items beside typed item failures. Structured JSON is bounded to 60,000 UTF-16 units and the complete envelope to eight MiB. Arrays shorten only at whole-item boundaries with explicit continuation or partial status.

At most eight whole calls are admitted. Each call receives 44 seconds for browser/image work, reserving 11 seconds for cleanup and result assembly inside a 55-second watchdog. An outer safety fallback can wait five more seconds while retaining admission ownership. Admission remains owned until execution and bounded cleanup finish.

## Verification boundary

Schema, Rust, browser-script, and client tests establish different properties. They do not prove live catalog access, account identity, seller-offer coverage, or recommendation quality. A disposable real-Chrome lifecycle run recorded seven passing tests in 8.45 seconds: cancellation followed by restart, strict profile contention, persistent poison state after cleanup failure, confirmed-close recovery, and private profile paths without fallback. It was compiled before the final source/tools changes and did not test abnormal broker termination. Release publication does not establish the remaining live broker, catalog, target-client, reconnect, and agent-task acceptance; those require separately recorded evidence.

```sh
UV_CACHE_DIR=/tmp/ozon-contracts-uv uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

`rust/page.ts` generates tracked `rust/page.js`; keep them together. Ordinary checks do not contact Ozon.

## Image DNS compatibility

Image downloads accept only HTTPS `ir.ozone.ru` and pin validated public destination addresses. If every system DNS answer belongs to the synthetic `198.18.0.0/15` VPN range, a bounded fallback queries only this constant CDN hostname through [Google Public DNS over HTTPS](https://developers.google.com/speed/public-dns/docs/doh/json). The resolver TLS hostname is pinned to a public bootstrap address; product URLs, queries, account data and cookies are never sent to it, and EDNS client subnet is disabled. DNS questions, CNAME chains and final public addresses are validated before the original CDN TLS connection. Other private or mixed answers remain rejected. Set `OZON_IMAGE_DOH_FALLBACK=off` to disable the fallback; the default is `auto`. No operating-system DNS or VPN settings are changed.

The broker owns idle cleanup. The driver's independent idle timer is disabled so it cannot terminate Chromium behind the broker while local journal calls keep the broker active. An abnormal broker exit leaves a private ownership marker. Startup can be interrupted before its CDP endpoint is captured, so a null endpoint is an incomplete acquisition rather than proof that no browser exists.

Recovery without a captured CDP endpoint must use the existing private driver connection, never a normal `agent-browser close` or `get cdp-url` command: the CLI can start a daemon, and CDP discovery can launch a browser. In pinned agent-browser 0.36.0, direct IPC `session_info` and `close` are explicitly exempt from browser launch. Recovery verifies the driver identity, requests closure, and retains ownership until shutdown is confirmed. Missing or inconsistent identity, an unresponsive driver, or an unconfirmed browser exit must preserve the marker and profile lock. Profile data, research history, and the driver's tab binding are not recovery debris and must not be deleted.

New ownership markers bind recovery to the canonical profile and its filesystem identity. The closing phase is persisted atomically before sending the close command, so a broker restart can finish an interrupted cleanup after the driver has disappeared. Marker removal still requires verified process absence. An old marker without enough ownership evidence remains blocked rather than authorizing closure of an unidentified browser.

Browser command timeouts use the existing `UPSTREAM_TIMEOUT` contract with `retryable: true` and `recovery: retry_later`; they do not imply that Ozon changed its page structure. A failed cleanup remains a separate failure and must not be disguised as a successfully recovered timeout.

Browser recovery validation on 2026-09-19: 158 Rust tests passed (four opt-in/helper tests ignored), the 18 page tests and all schema fixtures passed, and formatting plus strict Clippy passed on Rust 1.95.0/macOS. The separate real-driver test passed with agent-browser 0.36.0 and a disposable Chromium profile: incomplete acquisition, launch-free recovery, relaunch, and confirmed shutdown, using only `about:blank`. Regression tests also cover cancellation, interrupted closing, a surviving driver after browser exit, and rejected recovery identities. This validates the local implementation; it does not update installed clients or prove live Ozon access with the new binary.

## Validation on 2026-09-13

The optimized local binary completed 30 schema-validated MCP calls across six search categories with two simultaneous clients. Product and review images were returned as MCP image content, hash-checked, decoded, saved, and visually inspected. Search continuation and cached review continuation worked. Killing the explicitly owned test broker and reconnecting the same clients recovered the captured browser, preserved research evidence, and retained exactly one idempotent note event. The retained local report is `artifacts/vnext/live-e/report.md`; the test is reproducible with `scripts/live-smoke.py --crash-restart`. This is a bounded integration run, not a measurement of catalog recall or best-product recommendation quality.

The quota controls retained live SQLite pages (512 MiB). WAL, checkpoint, and compaction bookkeeping may require additional transient filesystem space; a strict instantaneous database-plus-WAL filesystem cap is not promised. Admission, retention, leases, and caller changes are checked transactionally, so a failed quota admission rolls them back together.

Remaining readiness limits: seller-offer listing is unsupported in the observed public source; changing between authenticated accounts in the same city may be unobservable; manually switching two real regions and the planned judged agent-evaluation pool have not been accepted. Existing Codex/Hermes client installations were not replaced by this repository implementation.


### Recovering a lost Ozon page

A warm driver can survive Chromium exit and recreate a browser on an empty or new-tab page. A cached readiness flag does not prove that the active page still belongs to Ozon. The page bridge reports `INVALID_ORIGIN` in this state; it is not evidence that a product disappeared or that Ozon presented a CAPTCHA.

For an invalid-origin header-context or API-fetch outcome, the page layer performs at most one navigation to the fixed public Ozon home page and one repeat of that evaluation, within the request cancellation and time budget. A repeated invalid origin remains a typed origin failure. Page-bound widget, modal, and navigation evaluations fail with the typed origin error instead of substituting home-page data. Failed page outcomes do not renew the warm readiness lease. This recovery does not sign in, change the delivery region, or bypass an access challenge.

Version 2.0.2 validation on 2026-09-19: 163 Rust tests passed (four opt-in/helper tests ignored), including warm-context recovery, shared fetch recovery, bounded repeated invalid origin, rejection of home-page substitution for page-bound modes, and failed-outcome lease behavior. Formatting and strict Clippy passed. A live acceptance run on the final candidate deliberately navigated the MCP-owned Ozon tab to `about:blank` and then fetched both previously failing products: both returned `ok`, preserving the authenticated verified Moscow context. One description remained `unknown`; this successful recovery does not imply complete product data or eliminate other marketplace failures.
