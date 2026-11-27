# Buyer research depth 3.1 acceptance — 2026-11-25

Implementation and the final deterministic gate are complete; live source acceptance remains limited. Public `schemaVersion` remains `3`; strict clients must rediscover the additive tool contracts. Baseline commit: `aa7b18f`, package 3.0.0. Candidate package: 3.1.0, local working tree; not published or activated in an existing client.

## Evidence boundaries

Deterministic fixtures establish extraction, bounded responses, provenance, local expansion and comparison behavior. They do not establish live Ozon availability, catalog coverage, recommendation quality, or model-token cost. Source shapes for the additions came from pinned public implementations listed in [the research](../research/ozon-research-depth-2026-11-25.md); synthetic fixtures are not disguised live captures.

A direct disposable-profile probe of the base card, supplemental card, full characteristics, offers and reviews received HTTP 403 on every path. No private browser profile, account login or anti-bot bypass was used. Full live acceptance must therefore remain separate from offline success.

## Initial measurements

The saved baseline executable published eight tools through actual MCP stdio discovery: 73,607 UTF-8 bytes / UTF-16 units in a compact JSON tools-list result; one invocation took 0.681 seconds. These are payload and wall-clock measurements, not model tokens or a latency distribution. Schema pruning alone, before adding the new capabilities, reduced the standalone schema set from 72,984 to 44,163 bytes (39.49%). The final expanded nine-tool inventory is measured below.

A disposable real Chromium lifecycle check passed: one test, 2.05 seconds test execution (0.11 seconds Cargo preparation). It exercised owned profile/process lifecycle and cancellation on local pages. It did not establish live source availability or visual focus behavior.

## Artifacts

Local task artifacts are under `.work/ozon-research-depth/validation/`. The baseline executable and identity are in `baseline/`; MCP discovery is in `baseline/discovery/`. The disposable probe is in `source-probe/`, and the real-browser check is `chromium-lifecycle.log`.

The first integrated check in `integrated-1/` found two source golden-fixture mismatches, unformatted in-progress application changes, and Clippy findings. Its failed result is retained; it is not the final gate. Independent review found additional identity, redirect, optional-failure, provenance and response-budget cases; fixes and final results will be recorded below.

## Integration measurements

The expanded nine-tool MCP `tools/list` result is 63,761 UTF-8 bytes / UTF-16 units, versus 73,607 for eight baseline tools: 9,846 fewer bytes (13.38%) while adding capabilities. The single measured discovery invocation took 0.719 seconds; the small timing difference is not evidence of a speed improvement. Actual model token counts and end-to-end shopping quality were not measured.

The controlled application fixture coalesced duplicate SKU selectors into one source call; saved-product comparison added zero product calls. One review source call returned a compact excerpt, with full text retrievable locally. The observed product response was 2,305 UTF-16 units / 2,347 UTF-8 bytes; the excerpted review response was 1,218 units / 1,330 bytes. The combined fixture took 103 ms. These measurements describe a synthetic workload, not live HTTP counts or a before/after latency comparison (`application-vertical.log`).

The second integrated gate passed all six stages in 12.377 seconds (`integrated-2/results.json`); additional review-driven envelope fixes followed, so the final gate below supersedes that candidate. Twelve Python semantic/replay tests passed separately (`python-tests.log`).

The proper two-client disposable MCP smoke returned PARTIAL in 26.609 seconds (`live-final/report.json`): both clients discovered the schema inventory, local research listing succeeded, and shared context was observed, but search returned `SOURCE_BLOCKED`. No product, review or image live acceptance can be claimed from this run. Cleanup reported no failures. This run preceded the last application-only response-budget fixes; its executable hash is retained in the report.

## Final deterministic acceptance

Command: `PATH="$PWD/.work/full-diagnostic/toolchain/installed/bin:$PATH" .work/ozon-research-depth/tooling/python-env scripts/check.py --offline --artifacts .work/ozon-research-depth/validation/integrated-final`.

| Stage | Result | Seconds |
| --- | --- | ---: |
| Rust formatting | PASS | 0.234 |
| Generated browser artifact | PASS | 0.248 |
| Browser VM tests | 26 passed | 0.252 |
| Contracts | 20 schemas, 13 positive / 29 negative cases passed | 0.280 |
| Rust tests | 189 passed, 1 opt-in lifecycle test ignored | 4.539 |
| Clippy all targets, warnings denied | PASS | 1.506 |
| Total | PASS | 7.059 |

The ignored lifecycle test was run separately and passed as recorded above. Python semantics/replay checks also passed: 12 tests, 0.024 seconds (`python-tests-final.log`). Final debug build succeeded in 2.20 seconds (`build-final.log`). No dependency was added or upgraded.

Final binary SHA-256: `91f43ecdb33b895dab13910ef6c5190083b94379254253d982a312019533592f`. `candidate-identity.json` records the local source manifest. Actual stdio discovery on this rebuilt artifact again published nine tools / 63,761 bytes in 0.776 seconds (`discovery-accepted-candidate/`).

Independent reviews covered comparison/contracts and cross-layer source identity, reply endpoint restrictions, cancellation, optional failure, read accounting, provenance, and output budgets. Material findings were corrected: exact modal SKU binding; redirect rejection; partial optional reply failures; source and local continuation; scalar evidence for offers and star buckets; variant price/availability provenance; retained source truncation; and measured pagination including reply metadata. Boundary fixtures test large rows and advancing cursors rather than treating an empty looping cursor as success.

The complete implementation includes optional full characteristics, variants and offers; richer reviews and official replies; local full-text/product expansion; historical comparison with exact requirements and verified quantity arithmetic; logical source budgets and duplicate-SKU coalescing; and agent instructions for diverse alternatives with one primary recommendation. This is locally built code, not a published release or an activated replacement in a running user client.

The final executable's two-client smoke also returned PARTIAL in 26.745 seconds (`live-accepted-candidate/report.json`), with the exact SHA above. Discovery, local listing and shared context worked; live search returned `SOURCE_BLOCKED`, with no cleanup failures. Thus the final code is deterministically verified, while new live characteristics/offers/replies, live images and end-to-end shopping recommendations remain unverified. There was no change to the user's active profile or MCP configuration.

A final test-only refinement populated variant price and availability in the existing mixed-selector application test: it passed (3.40 seconds compilation, 0.08 seconds execution; `populated-variant.log`). The assertions preserve the displayed price's unknown payment condition and verify availability through public response/evidence validation. Production code did not change after the final gate/build/live run.

## Correction: primary launch flow — 2026-11-26

The HTTP 403 observations above were from fresh disposable **headless** profiles. They are not evidence of a regression in the previously working flow. The prior `.work/full-diagnostic/report.md` records visible Chrome for Testing with an existing authenticated profile: 37 calls / 22 checks passed; headless was already known to receive 403 even after sign-in. On 2026-11-26 the connected working-profile `ozon_get_context` again reported available access, authenticated state and a verified region. This context check does not validate the new 3.1 source paths.

The owner selected visible Chrome as the primary flow. Runtime defaults and the disposable smoke now use visible mode; `OZON_HEADLESS=true` and smoke `--headless` are explicit diagnostics. The configured client already has `OZON_HEADLESS=false` and Chrome for Testing, so no active profile or client configuration change was necessary. Future main-flow acceptance must retain these working launch/profile conditions. Disposable smoke remains a separate check.

Visible-default follow-up verification: the required six-stage gate passed in 17.576 seconds (`.work/visible-default/checks/results.json`), including 189 Rust tests and 26 browser VM tests. All 12 Python checks also passed (`.work/ozon-research-depth/validation/visible-default-python.log`). The optional real headless lifecycle test now sets headless explicitly rather than inheriting the default. No fresh-profile live smoke was substituted for the working-profile context check.
