# ADR 0001: One v3 response and a separate journal

Status: Accepted on 2026-09-30.

## Context

The v2 compact, comparison, full, and delta representations made observation semantics depend on presentation choices and historical baselines. Independently written references and events could disagree. Retaining these compatibility paths would preserve the same ambiguity in a replacement journal.

## Decision

Use public `schemaVersion: "3"` with one lean response, explicit section selection, row `evidenceRefs`, and local expansion of evidence. Remove views, delta/novelty, and text-only compatibility. Persist each accepted operation's references, evidence, cursors, and event atomically in a new `journal.sqlite3` with schema `user_version = 1`.

Do not automatically migrate or alter the former `research.sqlite3`, profile, or artifacts. Old references and cursors are not imported into v3. Any history migration is separate work.

## Consequences

Clients must adopt v3 schemas and structured MCP results. Repeated observations return full rows rather than changes requiring historical reconstruction. Evidence details require a local journal read. Existing v2 history remains preserved, but it is not visible through the new journal by default. Atomic writes simplify reference authority and ensure failed validation does not expose partially committed observations.
