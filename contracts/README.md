# Ozon MCP contracts

This directory is the canonical machine-readable contract for the nine MCP tools. Schema version `3` is a breaking public contract change. Release 3.1.0 keeps `schemaVersion: "3"` and adds optional inputs and output fields. Clients pinned to strict earlier schemas must rediscover tool definitions before using the additions.

- `schemas/` contains Draft 2020-12 input and success-output schemas, the whole-tool failure schema, and `common.schema.json` with shared definitions.
- Canonical source schemas reference `common.schema.json#/$defs/name`. The server and checker compose those definitions into local `$defs`, reject namespace collisions, and rewrite references locally. Published MCP schemas are standalone; composition performs no network retrieval.
- `examples/positive/` contains valid input/output pairs and a failure example.
- `examples/negative/` contains structurally or semantically rejected cases.

Validation checks schema syntax, shared composition, local `$ref` resolution, formats, root objects, examples, and representative semantic invariants. It does not prove runtime behavior, browser extraction, Ozon access, or client compatibility.

Context capabilities describe static implementation support (`supported` or `unsupported`); region, account, and access fields describe the separately observed context. A supported operation does not prove live source availability or success.

All tools use one lean response. Row `evidenceRefs` connect observations to durable local evidence. The top-level `evidence` array is empty; retrieve source, time, and recorded facts with `ozon_get_research` and `section: evidence`. Search observations remain available as immutable snapshots with `section: candidates` and one to 20 `productRefs`. Stored observations are historical and do not establish fresh prices. Response views, delta rows, novelty metadata, and text-only compatibility are removed.

Search and reviews omit facets by default; use `includeFacets: true` to request them. Search refinement pages use `start.refinementsCursor`, default to 12 entries, allow at most 100, and are capped at 8,000 UTF-16 units per cached page. Item cursors retain the query and filters; refinement cursors also retain the refinement limit. Local research listing cursors retain their saved query; a continuation omits `query`.

`ozon_get_products` takes `products`, an array of 1–8 selector objects. Each selector has exactly one of `productRef`, `sku`, `url`, or `cursor`. For example, `{"researchId":"research-01","products":[{"productRef":"product-123"}],"include":["fullCharacteristics","variants"]}`. The default section is short `characteristics`; explicit sections are `characteristics`, `fullCharacteristics`, `description`, `variants`, `images`, `offers`, and `customsDuty`. `characteristicNames` selects returned characteristic names and does not reduce the source read; the full stored snapshot remains locally expandable. `maxSourceReads` limits logical source reads for the whole call. It excludes HTTP retries and context checks and does not mean total network requests. A section continuation uses `{"products":[{"cursor":"..."}]}` and omits selectors and read settings; the cursor retains its section and selection. Results preserve selector order and per-item errors. `productRefs` and `start.productRefs` are not product input fields.

Variants may include observed displayed price and availability; payment conditions remain unknown unless explicitly sourced. Offer rows retain their own SKU and seller. An Ozon-related offer is not proof of the same configuration as the original candidate. Offer-reading capability and observed offer availability are separate. A source may report partial/truncated coverage without a cursor when no continuation URL was observed.

`ozon_get_reviews` returns full text when it fits the row budget if `textLimit` is omitted; oversized text is explicitly marked `textTruncated` and retains its full `textLength`. Set `textLimit` to bound text on returned rows; full saved review text remains available locally through `ozon_get_research`, section `reviews`, selected by `reviewRefs`, with `textOffset`/`textLimit` for expansion. This journal read performs no market request. `includeDetails` and `includeSellerReplies` are opt-in. Seller replies are limited to official seller or brand replies. Their bounded excerpts carry an explicit `truncated` flag; full reply text is retained as chunked local evidence. Usage reports count logical source reads, excluding retries and context checks, not the total number of network requests.

`ozon_compare_products` compares 2–20 product references from saved observations in one research and performs no live reads. Requirements remain explicit rows with match, mismatch, or unknown status; the comparison does not apply a hidden overall score. Characteristic names and values compare exactly, including case and whitespace. Unit aliases are accepted only when interpreting quantities; a unit-price result needs same-research evidence for an unambiguous product quantity, and conflicting quantity observations remain unknown. Observation timestamps remain visible because the comparison is historical.

`ozon_get_research` defaults to `summary`. Paged `events`, `notes`, and `evidence` accept `limit` from 1 to 25, with defaults of 25, 10, and 20 respectively. Byte budgets can return fewer whole records with continuation. To retrieve exact notes, use `section: notes` with one to 10 `noteIds`; exact selection excludes `cursor` and `limit`. Exact evidence and candidate selection likewise exclude paging fields. Summary does not accept a cursor or limit.

Use `section: products` with one to eight saved `productRefs` to expand immutable product snapshots without a new Ozon read. `characteristicNames` can select characteristic rows. Use `section: reviews` with one to eight `reviewRefs` to retrieve saved full text; the optional offset and limit page within that text.

```sh
uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

The checker resolves its data root to this directory regardless of the current working directory.
