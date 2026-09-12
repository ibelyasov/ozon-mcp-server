# Issue tracker: GitHub

Issues and specs live in GitHub Issues for `ibelyasov/ozon-mcp-server`.
Use the `gh` CLI from this repository.

## Conventions

- "Publish to the issue tracker" means create a GitHub issue.
- "Fetch the relevant ticket" means read the issue, its labels, and comments.
- Use `gh issue` to create, read, list, comment, label, and close issues.
- For multiline bodies, write the exact text to a temporary file and use
  `--body-file`.
- Use the vocabulary in `docs/agents/triage-labels.md`.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Wayfinding operations

- Keep the map in one issue labelled `wayfinder:map`.
- Link child tickets as sub-issues. If unavailable, use a task list in the
  map and a `Part of #<map>` pointer in each child.
- Label children `wayfinder:<type>`:
  `research`, `prototype`, `grilling`, or `task`.
- Represent blockers using native issue dependencies. If unavailable,
  record `Blocked by: #<n>` in the child.
- Select the first open, unassigned child in map order whose blockers
  are all closed.
- Claim by assigning the ticket to the driving developer.
- On resolution, comment with the result, close the child, and append
  a summary and link to the map's Decisions-so-far.
