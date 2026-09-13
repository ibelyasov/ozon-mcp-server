# Configuration and behavior

[Русский обзор](../README.md) · [English overview](../README.en.md) · [Contracts](../contracts/README.md)

This describes development version 1.0.0, built with Rust 1.95, `rmcp` 3.2, and native `agent-browser` 0.36.0.

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

The broker and IPC support macOS and Linux. Windows is unsupported. The server never automatically signs in or changes region. To configure either, set `OZON_HEADLESS=false` before broker startup and request context from an MCP client, then make the change manually in the visible window. A running broker retains its launch configuration; stop it gracefully before changing mode.

The profile can contain cookies. The journal excludes credentials, cookies, account identifiers, full addresses, raw DOM/widgets, and unrestricted stderr. Keep both directories outside the repository.

## Context, observations, and references

`ozon_get_context` reports observed access, account, region, and capability state. `contextId` identifies the broker generation shared by agents. The live source reads the city only from the exact first cell observed on supported Ozon pages; a probe verified the label `Москва`. This establishes that observed city signal, not arbitrary region correctness. Switching between accounts in the same city may be unobservable, so `contextId` does not guarantee account identity.

Opaque `researchId`, `productRef`, `imageRef`, and `evidenceRef` values are authority only within private IPC. References bind to their research and context. Navigation and section cursors also bind filters and projection and expire after 30 minutes. Context changes invalidate navigation rather than mixing observations.

Search, product, review, and image calls only read Ozon and write bounded local journal records. Results preserve observation time, context, source association, and evidence paths. Unknown fields stay unknown; a missing or malformed source section is not a confirmed empty list.

Prices use integer minor units. `ozon_card` is a distinct observed payment condition and is never replaced with `regular`. A visible price does not prove account eligibility. Delivery labels remain as displayed and are not guarantees; an unknown delivery fee is not zero.

Search refinements represent complete observed states. Agents follow returned references instead of constructing category-specific URLs. Pagination is bounded and dynamic. `hasNext: false` requires a confirmed end; local limits produce partial or unknown coverage.

Product batches preserve selector order and per-item errors. Characteristics retain source labels and values. Variants are implemented from observed SKU mappings. A public live probe found no safe other-offers link, so seller offers remain explicitly `unsupported`. Product-section continuations use a bounded cached snapshot and preserve its original observation timestamp. Reviews preserve aggregation scope; review photos are implemented through research-bound image refs without inventing purchase filters.

Images accept only stored refs associated with the same research. Fetches enforce HTTPS, allowed origin, redirect and destination checks, raster MIME and decode, one MiB per image, four MiB per call, and a 20-megapixel decoded-source limit. Each returned image has a content index and hash; one failure does not hide successful siblings.

## Journal and errors

Research events, evidence, product references, and notes survive broker restart. Summaries include at most 100 product refs; every candidate remains enumerable through events. Page caps are 25 events, 20 evidence entries, and 10 notes.

Retention is 30 days with a 512 MiB logical database plus WAL budget. A request lease protects active research for at least ten idle minutes; no research is permanently active. Maintenance removes oldest idle research as a whole. If protected records prevent recovery, writes fail with `STORAGE_FULL`. A marketplace result cannot succeed if its journal write fails.

`ozon_append_research_note` atomically validates refs against the same research. `(researchId, operationId)` is idempotent for canonical identical JSON; changed reuse returns `CONFLICT`. Source text and notes are untrusted data, never instructions.

Whole-tool failures use a stable typed code and omit success `structuredContent`. Batch tools may preserve successful items beside typed item failures. Structured JSON is bounded to 60,000 UTF-16 units and the complete envelope to eight MiB. Arrays shorten only at whole-item boundaries with explicit continuation or partial status.

At most eight whole calls are admitted. Each call receives 44 seconds for browser/image work, reserving 11 seconds for cleanup and result assembly inside a 55-second watchdog. An outer safety fallback can wait five more seconds while retaining admission ownership. Admission remains owned until execution and bounded cleanup finish.

## Verification boundary

Schema, Rust, browser-script, and client tests establish different properties. They do not prove live catalog access, account identity, seller-offer coverage, or recommendation quality. A disposable real-Chrome lifecycle run recorded seven passing tests in 8.45 seconds: cancellation followed by restart, strict profile contention, persistent poison state after cleanup failure, confirmed-close recovery, and private profile paths without fallback. It was compiled before the final source/tools changes and did not test abnormal broker termination. Version 1.0.0 remains in development until the remaining live broker, catalog, target-client, reconnect, and agent-task gates have recorded evidence.

```sh
UV_CACHE_DIR=/tmp/ozon-contracts-uv uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

`rust/page.ts` generates tracked `rust/page.js`; keep them together. Ordinary checks do not contact Ozon.

## Image DNS compatibility

Image downloads accept only HTTPS `ir.ozone.ru` and pin validated public destination addresses. If every system DNS answer belongs to the synthetic `198.18.0.0/15` VPN range, a bounded fallback queries only this constant CDN hostname through [Google Public DNS over HTTPS](https://developers.google.com/speed/public-dns/docs/doh/json). The resolver TLS hostname is pinned to a public bootstrap address; product URLs, queries, account data and cookies are never sent to it, and EDNS client subnet is disabled. DNS questions, CNAME chains and final public addresses are validated before the original CDN TLS connection. Other private or mixed answers remain rejected. Set `OZON_IMAGE_DOH_FALLBACK=off` to disable the fallback; the default is `auto`. No operating-system DNS or VPN settings are changed.

The broker owns idle cleanup. The driver's independent idle timer is disabled so it cannot terminate Chromium behind the broker while local journal calls keep the broker active. An abnormal broker exit leaves a private captured-CDP marker; the next owner must recover that exact browser or fail closed.

## Validation on 2026-09-13

The optimized local binary completed 30 schema-validated MCP calls across six search categories with two simultaneous clients. Product and review images were returned as MCP image content, hash-checked, decoded, saved, and visually inspected. Search continuation and cached review continuation worked. Killing the explicitly owned test broker and reconnecting the same clients recovered the captured browser, preserved research evidence, and retained exactly one idempotent note event. The retained local report is `artifacts/vnext/live-e/report.md`; the test is reproducible with `scripts/live-smoke.py --crash-restart`. This is a bounded integration run, not a measurement of catalog recall or best-product recommendation quality.

The quota controls retained live SQLite pages (512 MiB). WAL, checkpoint, and compaction bookkeeping may require additional transient filesystem space; a strict instantaneous database-plus-WAL filesystem cap is not promised. Admission, retention, leases, and caller changes are checked transactionally, so a failed quota admission rolls them back together.

Remaining readiness limits: seller-offer listing is unsupported in the observed public source; changing between authenticated accounts in the same city may be unobservable; manually switching two real regions and the planned judged agent-evaluation pool have not been accepted. Existing Codex/Hermes client installations were not replaced by this repository implementation.
