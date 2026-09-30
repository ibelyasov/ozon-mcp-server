## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues. Before tracker operations, read `docs/agents/issue-tracker.md`.

### Triage labels

Use the five canonical triage labels. Before triage, read `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout. Before exploring the codebase, read `docs/agents/domain.md`.

### Checks

Before completing code changes, run `python scripts/check.py`; add `--offline` when dependencies are cached. See `README.md` for pinned check dependencies and `docs/behavior.md#verification` for acceptance boundaries. Report the saved logs and measured timings. Real Chromium and live Ozon acceptance require their explicit disposable checks.
