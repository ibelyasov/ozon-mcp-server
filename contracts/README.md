# Ozon MCP contracts

This directory is the canonical machine-readable contract for the eight version 1.0.0 tools.

- `schemas/` contains standalone Draft 2020-12 input and success-output schemas plus the whole-tool failure schema.
- `examples/positive/` contains valid input/output pairs and a failure example.
- `examples/negative/` contains structurally or semantically rejected cases.

Validation checks schema syntax, local `$ref` resolution, formats, root objects, examples, and representative semantic invariants. It does not prove runtime behavior, browser extraction, Ozon access, or client compatibility.

`ozon_search`, `ozon_get_products`, and `ozon_get_reviews` expose `compact` (default), `comparison`, and `full` response views. Compact and comparison responses keep row `evidenceRefs` while moving inline evidence detail to the journal; callers can expand selected references with `ozon_get_research` and `section: evidence`. Full is the compatibility view with inline metadata. Search baselines are likewise available with `section: candidates` and one to 20 `productRefs`. Treat a field omitted by a view as omitted presentation data, not as an observed `unknown` value.

Search supports `repeatMode: full | delta`; delta retains every result row, decision-critical price, availability, and range-match fields, plus changed fields and a `baselineProductRef`. Refinement pages use `start.refinementsCursor`, default to 12 entries, allow at most 100, and are capped at 8,000 UTF-16 units per cached page. Item cursors bind view and repeat mode; refinement cursors also bind the refinement limit.

The default `OZON_MCP_TEXT_MODE=compact` returns a short text pointer and the complete success object once in `structuredContent`. `json` duplicates that object in text for legacy text-only clients. Errors are independent of this transport compatibility setting.

```sh
UV_CACHE_DIR=/tmp/ozon-contracts-uv uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

The checker resolves its data root to this directory regardless of the current working directory.
