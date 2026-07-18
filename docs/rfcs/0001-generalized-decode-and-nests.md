# RFC-0001: Generalized event decode and multi-contract nests

- Status: Implemented (2026-07-18)
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: — (first post-skeleton slice)
- Blocks: RFC-0002 (Horizon nest), RFC-0004 (backfill), RFC-0005 (v0.1.0)

## Abstract

Replace the hardcoded ERC-20 `Transfer` decode path with a general event-decode engine
driven entirely by resolved ABIs, and extend the nest model from one contract to many.
After this RFC, `nuthatch init <addr>... --chain <chain>` produces a nest that decodes
every event of every listed contract into per-event tables, with the same hot/cold,
sealing, and serving behavior the skeleton already has.

## Motivation

The binary indexes one event signature on one contract. The website's /example page and
the entire nest concept assume arbitrary contracts and events. This is the largest gap
between what Nuthatch claims and what it does, and it blocks the Horizon nest (RFC-0002),
honest backfill benchmarks (RFC-0004), and v0.1.0 (RFC-0005).

## Goals

1. Decode all non-anonymous events declared in any resolved ABI, for N contracts per nest.
2. Deterministic, versioned decode: identical inputs always produce identical rows, and
   the decode registry's content hash is recorded so re-execution is verifiable.
3. Schema generation: one logical table per (contract alias, event), stable naming.
4. All existing invariants hold: reorg rollback, finality-gated sealing, hot-store
   pruning, DuckDB cold serving, RAM budget.

## Non-goals

- Generalizing the DBSP/IVM layer to arbitrary SQL views (that is the semantic-layer
  slice; the existing hardcoded balance view remains and activates only when a nest
  contains an ERC-20 `Transfer`-shaped table).
- eth_call enrichment, traces, or state diffs. Events only.
- Proxy implementation resolution beyond what ABI resolution already does (tracked as an
  open question).

## Design

### 1. ABI ingestion and the decode registry

Parse each contract's resolved ABI with `alloy-json-abi`. Build one immutable
`DecodeRegistry` per nest at startup:

```
Registry: Map<topic0: B256, Vec<EventDecoder>>
EventDecoder {
  contract: Address,          // attribution filter; None for a future generic fallback
  alias: String,              // from nuthatch.toml
  event: alloy Event,          // full param types, indexed flags
  table: TableId,             // stable name, see §3
  registry_hash: [u8; 32],    // sha256 of canonical registry serialization
}
```

Lookup path per log: `registry[topic0]` then filter by emitting address. Contract-specific
decoders always precede any generic fallback (Allium pattern; fallback itself is out of
scope for this RFC but the ordering contract is fixed now).

Edge cases, decided:
- **Anonymous events**: skipped in v1, counted in a `skipped_anonymous` metric. They have
  no topic0 and cannot be safely attributed by signature.
- **Overloaded events** (same name, different params): disambiguated in table naming (§3);
  topic0 differs so decode is unambiguous.
- **Identical signatures across contracts** (e.g., `Transfer` on two tokens): one decoder
  per (contract, event); rows land in that contract's table. Same topic0, different
  address → different decoder.
- **Indexed dynamic types** (indexed string/bytes/arrays): the topic contains the keccak
  hash, not the value. Stored as `bytes32` with column suffix `_hash`; documented.

### 2. Type mapping (Solidity → storage → SQL)

Determinism rule: the canonical stored form is exact; convenience forms are derived,
never stored as truth.

| Solidity | Row encoding (hot + Parquet) | SQL surface (DuckDB) |
|---|---|---|
| address | 20-byte fixed binary | lowercase hex TEXT via view |
| uintN ≤ 64 / intN ≤ 64 | u64 / i64 | BIGINT/UBIGINT |
| uintN ≤ 128 / intN ≤ 128 | 16-byte BE fixed binary | HUGEINT via cast in view |
| uint256 / int256 | 32-byte BE fixed binary | canonical BLOB + `*_dec` DECIMAL(38) view column when it fits, else NULL with `*_overflow` flag |
| bool | u8 | BOOLEAN |
| bytesN | N-byte fixed binary | hex TEXT view |
| bytes / string | length-prefixed bytes / UTF-8 | BLOB / TEXT |
| arrays / tuples | canonical JSON text (v1) | TEXT; flagged `json` in schema. Revisit as Arrow lists post-v0.1 |

Every table gets implicit columns: `block_number: u64`, `block_hash: bytes32`,
`tx_hash: bytes32`, `log_index: u32`, `address: bytes20` (emitter), `_seq: u64`
(monotonic per-table insertion sequence for stable ordering).

### 3. Table naming and schema manifest

`{alias}__{event_snake_case}` (double underscore separates alias from event; aliases are
validated `[a-z][a-z0-9_]*`). Overloads append a 4-hex-char suffix of the topic0:
`pool__swap_a1b2`. The generated `schema.json` in the nest records, per table: topic0,
full event signature, column list with Solidity + storage + SQL types, and the registry
hash. `nuthatch mcp`'s `schema` tool and `llms.txt` are regenerated from this manifest —
one source of truth.

### 4. nuthatch.toml and init

```toml
[nest]
name = "my-nest"
chain = "mainnet"

[[contracts]]
alias = "usdc"
address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
start_block = 6_082_465        # auto-detected; overridable
abi = "abis/usdc.json"

[[contracts]]
alias = "pool_factory"
address = "0x1F98431c8aD98523631AE4a59f267346ea31F984"
start_block = 12_369_621
abi = "abis/pool_factory.json"
```

`init` accepts N addresses (and `--alias` pairs, else aliases derived from resolved
contract names, deduplicated). Per-contract ABI resolution keeps the existing
Sourcify-then-Etherscan order; ABIs are written to `abis/` (no runtime re-fetch — the
nest is self-contained). Deployment-block auto-detection: binary search on
`eth_getCode(addr, block) == 0x` over [0, tip], ~40 RPC calls per contract, cached in
the toml.

### 5. Ingestion changes

One `eth_getLogs` filter per poll/backfill chunk: `address: [all contract addresses]`,
`topics: [[all registry topic0s]]`. The existing chunking, round-robin failover, and
checkpointing are unchanged. Decode fan-out is a pure function `Vec<Log> →
Vec<(TableId, Row)>`; parallelize with rayon only if profiling shows need (non-goal to
optimize here; RFC-0004 owns throughput).

### 6. Storage generalization

Hot store (redb): replace the transfer-specific table with one redb table per event
table, key `(block_number, log_index)` big-endian concatenated for range deletes on
rollback, value = bincode row. `rollback_to(block)` iterates all tables (range delete
`> block` — cheap in redb, and the reason Daimo's Postgres pain doesn't apply here).
Checkpoints unchanged.

Sealing: one Parquet file per (event table, sealed range); `manifest.json` becomes
`{ tables: { table_name: [ {file, sha256, block_range, rows} ] } }`. DuckDB serving
creates one view per table over its segment glob. `/entity/{id}` generalizes to
`/table/{name}/row/{block}/{log_index}` with the old route kept as an alias for the
transfer table when present.

Watermark: `sealed_through` remains global (min across tables is unnecessary — all
tables ingest from the same block stream and seal together per range).

### 7. Serving

- `/tables` → list from schema manifest (replaces `/entities` semantics; keep
  `/entities` as deprecated alias for one release).
- `/table/{name}?limit&from_block&to_block` → hot + cold merged read.
- `/sql` unchanged (now sees all per-table views).
- MCP `schema`/`sql`/`entity` tools regenerate from the manifest; `balance`/
  `top_balances` register only when the balance view is active.

## Implementation plan

1. `DecodeRegistry` + type mapping + golden decode tests (fixtures: USDC Transfer/
   Approval, Uniswap V3 `PoolCreated` (address-heavy), a `bytes`/`string` event, an
   overloaded pair, an indexed-string event). Registry hash in schema manifest.
2. Multi-contract init (+ deployment-block detection) and `nuthatch.toml` v2 with a
   migration shim for v1 files.
3. Generic hot store + rollback; proptest updated to multi-table convergence.
4. Sealing/manifest/DuckDB per-table; pruning; point-read fallback across the seam.
5. Serving + MCP + llms.txt regeneration from the manifest.
6. Footprint re-measurement with a 3-contract nest; update README table and CI scenario.

## Testing and acceptance

- Golden: fixture logs → exact rows, for every type in §2's table.
- Property: encode/decode roundtrip per type; multi-table reorg convergence at random
  fork depths.
- End-to-end: `init` with USDC + Uniswap V3 factory + WETH on mainnet, live poll,
  `/sql` joins across two tables, pruned-row fallback works per table.
- Budget: peak RSS with the 3-contract nest ≤ 256 MB in the CI scenario (product budget
  2 GB untouched).
- Acceptance gate for RFC-0002: the three Horizon contract ABIs decode with zero
  skipped (non-anonymous) events.

## Risks

- **u256 ergonomics**: BLOB canonical form makes ad-hoc SQL clumsy; mitigated by
  generated `*_dec` view columns. Revisit if DECIMAL(38) truncation warnings are common
  in practice (Horizon's GRT amounts fit in 38 digits; fine).
- **Table explosion** for ABI-heavy contracts (hundreds of events): redb handles many
  tables; Parquet segment count grows linearly. Mitigate later with `events = ["..."]`
  allowlist per contract in toml (cheap to add; not v1-blocking).
- **Registry hash churn**: any ABI re-resolution changes the hash and thus the
  verifiability claim. ABIs are vendored into the nest precisely to freeze this.

## Alternatives considered

- **One wide `events` table with JSON payloads** (shovel-style): simpler storage, but
  loses typed SQL, typed Parquet, and per-table pruning; rejected.
- **Codegen per nest (compile-time decode)**: faster, but breaks the single-binary
  `init → dev` flow with a compile step; rejected for the default path (a future
  `nuthatch build --aot` could revisit).

## Open questions

1. Proxy contracts: resolve implementation ABI via Sourcify's proxy metadata at init
   (follow EIP-1967 slots via eth_getStorageAt)? Deferred; document workaround
   (`--abi-from <impl-addr>` flag, trivial to add).
2. Should `_seq` be global rather than per-table (total ordering across tables)?
   Per-table suffices for current serving; revisit for streaming (post-v0.1).
