# Ozon Product Research

This dictionary keeps marketplace observations, agent judgment, and local continuity distinct.

## Language

**Research**: A bounded, durable body of observations and agent notes for one shopping need in one Context.
_Avoid_: Session, search history

**Context**: The observable account-and-region setting shared by connected agents. It may not distinguish accounts that expose the same signals.
_Avoid_: Profile, user identity

**Observation**: Marketplace data captured at a stated time and Context, without implying that it remains current or complete.
_Avoid_: Fact, truth

**Evidence**: Provenance connecting an Observation to an allowed source, recorded facts, and observation time.
_Avoid_: Proof, recommendation

**Candidate**: An observed SKU considered within a Research, whether selected, rejected, or uncertain.
_Avoid_: Product family, recommendation

**Product Reference**: An opaque Research-bound reference to an observed SKU. A reference identifies the observed candidate; it does not establish current stock or price.
_Avoid_: Product ID, URL

**Refinement Reference**: An opaque, expiring reference to one complete observed refinement state.
_Avoid_: Filter, search URL

**Cursor**: An opaque, expiring continuation bound to its Research, Context, and captured selection criteria.
_Avoid_: Page number, current catalog

**Agent Note**: An append-only local statement of requirements, assessment, or conclusion attributed to the agent.
_Avoid_: Observation, evidence

**Ozon Card Price**: A price observed with an explicit Ozon Card payment condition.
_Avoid_: Discount price, current price

**Coverage**: A bounded account of what a retrieval chain examined and returned, including known losses and uncertainty.
_Avoid_: Total catalog, exhaustive recall

**Capability**: Whether an operation is supported by the implementation, distinct from observed access or the outcome of a particular request.
_Avoid_: Availability, readiness

**Broker**: The single local owner of marketplace access and the shared Context for connected frontends.
_Avoid_: MCP client, browser session
