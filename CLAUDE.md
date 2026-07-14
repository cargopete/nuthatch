# Nuthatch — CLAUDE.md

Nuthatch is a self-hosted-first, AI-native blockchain indexer. One Rust binary, one command,
live indexed API in under two minutes. No mandatory third-party data dependency, ever.
Tagline: "be your own indexer."

This file is the standing brief. Read it before any task. When a task conflicts with the
non-negotiables below, stop and flag it instead of proceeding.

## Non-negotiables

1. **Single static binary** is the primary deliverable. Embedded mode must run with zero
   external services: no Postgres, no Docker, no IPFS. `curl | sh` → `nuthatch init 0xAddr
   --chain mainnet` → `nuthatch dev` → live API. Target: <2 minutes to first indexed query.
2. **Footprint budget: ≤2 GB RAM** for single-chain tip-following + serving in embedded mode.
   Treat this as a CI-enforced budget, not an aspiration. If a design decision threatens it,
   surface the tradeoff before implementing.
3. **No phone-home.** No telemetry, no mandatory API tokens, no gated data services. AI
   features use local models (Ollama) or BYO API key, and degrade gracefully offline.
4. **Determinism in the core.** ABI decoding, reorg handling, entity derivation, and anything
   feeding stored state must be deterministic and re-executable. LLMs generate code and tests;
   LLM output never sits in the runtime data path.
5. **AGPL-3.0** for the core. Do not vendor or port code from AGPL projects we don't own
   (notably SQD's worker-rs) — read for ideas only. Safe dependencies: reth (MIT/Apache),
   Cryo (permissive), Feldera/DBSP (MIT OR Apache-2.0), DataFusion/Arrow/DuckDB (Apache-2.0).
   Do NOT add Materialize (BSL) or any Envio/HyperSync dependency.

## Architecture (two modes, one codebase)

**Embedded mode (default):** single process. Ingestion (RPC extraction with aggressive
batching, Cryo-style; optional reth ExEx when colocated with a node) → deterministic decode →
hot tip store (redb) for entity point-reads → sealed content-addressed Parquet segments past
finality → DuckDB attaching segments **read-only** for analytical SQL. DuckDB is single-writer:
only the ingestion thread writes; queries attach read-only. Never design around concurrent
DuckDB writers.

**Scaled mode (docker-compose):** same crates, Postgres replaces redb for the hot store,
DataFusion federates hot + cold behind one SQL surface. Feature-flag the storage backend
behind a trait; no `#[cfg]` forks of business logic.

**Reorg strategy:** reorgs only ever touch the mutable hot store. Segments are sealed to
Parquet strictly past finality, so the columnar layer is append-only and immutable. If a
change requires mutating sealed segments, the design is wrong — go back.

**Entity derivation:** two authoring modes.
- Declarative (default): entities as incremental views over decoded events, maintained by
  DBSP (Feldera crates). This is the differentiator — reorgs become retractions, backfills
  become batch runs of the same circuit.
- Imperative (escape hatch): WASM component handlers, per the transform layer below.

## The transform layer: lessons from liminal (lodestar-team/liminal)

Liminal is the prototype for Nuthatch's transform runtime. Study `liminal-host/`, `wit/`, and
`liminal-sdk/` before writing any transform-layer code. Port the design, not just the idea.

**Adopt directly:**
- WIT-first workflow: define/modify WIT interfaces before touching host or component code.
  Typed channels between stages; the WIT files are the API contract and get reviewed first.
- Per-component capability injection at composition time. The host grants `wasi:http`,
  key-value, filesystem per component, never per pipeline.
- **Purity by construction:** a component granted zero capabilities is deterministic by
  definition. Enforce the rule in the host: only zero-capability components may feed entity
  derivation / stored state. Effectful components (HTTP enrichers etc.) produce annotations
  only, never canonical entities. Purity must be checkable from the composition manifest —
  no code inspection required.
- Single cursor, single process, one observable failure boundary. Never introduce a second
  cursor or a reconciliation layer.
- Host owns orchestration, retries, and state; components are stateless pure stages.
- Optional sinks warn-and-skip when unconfigured (liminal's `--database-url` pattern) — apply
  this graceful-degradation pattern to every optional integration.
- Examples-as-documentation: every capability ships with a runnable example pipeline, in the
  style of liminal's `examples/uni-v3-swaps`.
- Wasmtime pinned, WASIp2 (`wasm32-wasip2`) now; track WASIp3 but do not adopt until stable
  in Wasmtime. Keep WIT interfaces p3-migratable (avoid patterns that only make sense in p2).

**Change from liminal (its known gaps for this workload):**
- **Batch the boundary.** Liminal's per-event component calls won't survive backfill targets
  (≥10K events/sec floor, aim 30K). WIT interfaces take batches — lists of events or
  serialized Arrow IPC buffers — never one event per call. Arrow is the interchange format
  everywhere; don't invent bespoke serialization.
- **Stateless components as a hard contract:** components are pure functions
  `batch of blocks → batch of facts`. All state lives host-side. Components never see reorgs
  and have no rollback interface; the host handles reorg via hot-store rollback and IVM
  retractions.
- Components are the escape hatch, not the front door: the `init` flow must produce a working
  indexer with zero user-written components (generated decode + declarative views).

## Correctness rules

- Decode: deterministic Rust, topic0-keyed, contract-ABI priority with generic fallback.
  ABI acquisition: Sourcify first, then Etherscan-class APIs. Cache ABIs locally.
- Never retroactively re-decode stored history when ABIs improve; version decodings.
- Golden/deterministic-simulation tests for every handler and view (Matchstick lineage):
  fixed block fixtures in, exact entity state out. AI-generated tests are welcome, but they
  must be deterministic and reviewed like any code.
- Property tests for reorg handling: random reorg depths against the hot store must always
  converge to the canonical chain state.
- Benchmarks are CI artifacts: backfill events/sec, tip lag ms, entity point-read p50/p99,
  RSS. Regressions fail the build.

## AI-native surface (built-in, sovereignty-respecting)

- MCP server compiled into the binary: schema discovery, SQL execution, entity lookup,
  streaming subscribe. Works fully offline against the local instance.
- `nuthatch init 0xAddr` scaffolds schema + views + handlers + tests from the ABI.
- Ship `llms.txt`, docs-as-MCP, and a `.claude/skills/` directory in scaffolded projects so
  coding agents get real syntax instead of hallucinating.
- Local-first AI: Ollama support and BYO-key. Any AI feature must have a documented
  no-network fallback or be clearly marked unavailable offline.

## Build order (vertical slices; each ends runnable)

1. Skeleton: single binary, config, `init` (ABI fetch → generated project), RPC ingestion,
   decode, redb hot store, HTTP serving of entity point-reads. One chain (Ethereum). This
   slice alone must hit the <2-minute demo.
2. Parquet sealing past finality + DuckDB read-only analytical SQL + reorg property tests.
3. DBSP declarative views (the IVM core) replacing hand-rolled entity updates.
4. Transform runtime ported from liminal with batched Arrow WIT interfaces.
5. MCP server + scaffolded skills + llms.txt.
6. ExEx ingestion mode (colocated reth), then scaled mode (Postgres/DataFusion).

Do not start slice N+1 while slice N has failing tests or an unmet budget.

## Out of scope — do not build, do not suggest

- Hosted service, billing, metering, multi-tenancy.
- Token, staking, decentralized network features (a possible future Graph Horizon data
  service is explicitly deferred).
- Non-EVM chains before EVM is airtight.
- TEE attestation, zk proofs (verifiability = deterministic re-execution of pure components
  + content-addressed segments; nothing heavier).
- Kubernetes manifests, Helm charts, or any deployment story beyond binary + compose.
