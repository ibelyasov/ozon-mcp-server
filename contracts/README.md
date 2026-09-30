# Ozon MCP contracts

This directory is the canonical machine-readable contract for the eight MCP tools. Schema version `3` is a breaking public contract change.

- `schemas/` contains Draft 2020-12 input and success-output schemas, the whole-tool failure schema, and `common.schema.json` with shared definitions.
- Canonical source schemas reference `common.schema.json#/$defs/name`. The server and checker compose those definitions into local `$defs`, reject namespace collisions, and rewrite references locally. Published MCP schemas are standalone; composition performs no network retrieval.
- `examples/positive/` contains valid input/output pairs and a failure example.
- `examples/negative/` contains structurally or semantically rejected cases.

Validation checks schema syntax, shared composition, local `$ref` resolution, formats, root objects, examples, and representative semantic invariants. It does not prove runtime behavior, browser extraction, Ozon access, or client compatibility.

Context capabilities describe static implementation support (`supported` or `unsupported`); region, account, and access fields describe the separately observed context.

All tools use one lean response. Row `evidenceRefs` connect observations to durable local evidence. The top-level `evidence` array is empty; retrieve source, time, and recorded facts with `ozon_get_research` and `section: evidence`. Search observations remain available as immutable snapshots with `section: candidates` and one to 20 `productRefs`. Stored observations are historical and do not establish fresh prices. Response views, delta rows, novelty metadata, and text-only compatibility are removed.

Search and reviews omit facets by default; use `includeFacets: true` to request them. Search refinement pages use `start.refinementsCursor`, default to 12 entries, allow at most 100, and are capped at 8,000 UTF-16 units per cached page. Item cursors retain the query and filters; refinement cursors also retain the refinement limit. Local research listing cursors retain their saved query; a continuation omits `query`.

`ozon_get_products` takes `products`, an array of 1-8 selector objects. Each selector has exactly one of `productRef`, `sku`, `url`, or `cursor`. For example, `{"researchId":"research-01","products":[{"productRef":"product-123"}],"include":["variants"]}`. The default section is `characteristics`; explicit sections can be `characteristics`, `description`, `variants`, or `images`. A section continuation uses `{"products":[{"cursor":"..."}]}` and omits `include`; the cursor retains its section. Both the published schema and server validation enforce that rule. Results preserve selector order and per-item errors. `productRefs` and `start.productRefs` are not product input fields.

`ozon_get_research` defaults to `summary`. Paged `events`, `notes`, and `evidence` accept `limit` from 1 to 25, with defaults of 25, 10, and 20 respectively. Byte budgets can return fewer whole records with continuation. To retrieve exact notes, use `section: notes` with one to 10 `noteIds`; exact selection excludes `cursor` and `limit`. Exact evidence and candidate selection likewise exclude paging fields. Summary does not accept a cursor or limit.

```sh
uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

The checker resolves its data root to this directory regardless of the current working directory.
