# Ozon MCP contracts

This directory is the canonical machine-readable contract for the eight version 1.0.0 tools.

- `schemas/` contains standalone Draft 2020-12 input and success-output schemas plus the whole-tool failure schema.
- `examples/positive/` contains valid input/output pairs and a failure example.
- `examples/negative/` contains structurally or semantically rejected cases.

Validation checks schema syntax, local `$ref` resolution, formats, root objects, examples, and representative semantic invariants. It does not prove runtime behavior, browser extraction, Ozon access, or client compatibility.

```sh
UV_CACHE_DIR=/tmp/ozon-contracts-uv uv run --offline --no-project --with 'jsonschema[format]==4.25.1' python scripts/validate-contracts.py
```

The checker resolves its data root to this directory regardless of the current working directory.
