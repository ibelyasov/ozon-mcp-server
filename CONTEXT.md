# Ozon Product Research

This context describes evidence-backed product research on Ozon and keeps marketplace observations, agent judgment, and local continuity distinct.

## Language

**Research**:
A bounded, durable body of observations and agent notes for one shopping need in one Context.
_Avoid_: Session, search history

**Context**:
The observable account-and-region setting shared by local agents through one Broker generation. It may not distinguish accounts that expose the same observable signals.
_Avoid_: Profile, user

**Observation**:
Marketplace data captured at a stated time and Context, without implying that it remains current or complete.
_Avoid_: Fact, truth

**Evidence**:
Provenance connecting fields in an Observation to an allowed source and observation time.
_Avoid_: Citation, proof

**Candidate**:
An observed SKU considered within a Research, whether selected, rejected, or uncertain.
_Avoid_: Product family, recommendation

**Product Reference**:
An opaque Research-bound reference to an observed SKU.
_Avoid_: Product ID, URL

**Refinement Reference**:
An opaque, expiring reference to one complete observed refinement state.
_Avoid_: Filter, search URL

**Cursor**:
An opaque, expiring continuation bound to its Research, Context, filters, and projection.
_Avoid_: Page number, snapshot

**Agent Note**:
An append-only local statement of requirements, assessment, or conclusion attributed to the agent.
_Avoid_: Observation, evidence

**Ozon Card Price**:
A price observed with an explicit Ozon Card payment condition.
_Avoid_: Discount price, current price

**Coverage**:
A bounded account of what a retrieval chain examined and returned, without claiming exhaustive catalog recall.
_Avoid_: Completeness, total catalog

**Broker**:
The single local owner of marketplace access and the shared Context for connected frontends.
_Avoid_: MCP server, browser session
