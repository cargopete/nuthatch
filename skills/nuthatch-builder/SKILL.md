---
name: nuthatch-builder
description: Build, configure, run, or debug a nuthatch indexer (a "nest" or "roost"). Use when the user wants to index a smart contract's events into a local SQL database with nuthatch — scaffolding from an address, editing nuthatch.toml/semantic.toml, adding contracts, factories, compliance screening, packaging/mounting nests, running a roost, or troubleshooting a running indexer.
---

# Building with nuthatch

nuthatch turns a contract address into a local SQL database: one Rust binary, one command, no
Postgres/Docker, no third-party data API, no bill. This skill teaches you to *drive* the CLI. For
*querying a running nest's data* (what a table means, what's in it as of block N), use the nest's MCP
`schema`/`sql` tools instead — that's runtime knowledge; this is authoring knowledge.

**Never fight the non-negotiables** (the binary enforces them; working against them is always wrong):
one writer, one cursor per chain (a second chain = a second process), sealed Parquet segments are
immutable (reorgs only ever touch the hot store), and the `/sql` guards (timeout, row cap, concurrency)
are node self-protection, not obstacles to remove.

## The 90-second happy path

```sh
nuthatch init 0xA0b86991c6218b36c1D19D4a2e9Eb0cE3606eB48   # USDC — chain auto-detected
nuthatch dev            # backfill from deployment, follow the tip, serve an API on :8288
nuthatch sql "SELECT count(*), sum(value_dec) FROM usdc__transfer"
```

- `init` resolves the ABI (Sourcify → Etherscan), writes `nuthatch.toml`, `schema.json`,
  `semantic.toml`, `llms.txt`, and a `.claude/skills/` scaffold. **Omit `--chain`** — it probes the
  known chains for the contract's bytecode. Pass several addresses to index them together.
- `dev` shows a live backfill progress line, then "caught up to tip". It *is* the serve command.
- `nuthatch sql` with no query opens a REPL (`.tables`, `.schema <t>`, history).

## When to read what

- **[cli-reference.md](cli-reference.md)** — GENERATED from the binary. The authoritative list of every
  subcommand and flag. If a flag isn't here, it doesn't exist — never invent one.
- **[config-reference.md](config-reference.md)** — every `nuthatch.toml` / `semantic.toml` / `roost.toml`
  key.
- **[config-as-code.md](config-as-code.md)** — the optional `nest.star` (Starlark) front-end: compute a
  repetitive or derived config in code instead of hand-writing TOML. Read only if a nest's config wants
  a loop or composition.
- **[workflows.md](workflows.md)** — the recipes: init→dev→sql, add a contract, factories, publish a
  nest (pack/mount), run a roost, wire an AI client.
- **[compliance.md](compliance.md)** — labels, sanctions lists, screening, flags, exposure, the signed
  audit pack (only relevant if the user asks for compliance features).
- **[troubleshooting.md](troubleshooting.md)** — symptom → `/metrics` series → remedy for tip lag, RPC
  failover, reorgs, guard rejections, and the RAM budget.

## Golden rules

1. **Read `cli-reference.md` before using a flag you're unsure of.** Hallucinated flags are the #1 way
   agents break nuthatch.
2. **One chain per process.** To index a second chain, run a second `nuthatch dev`. Never try to
   multiplex chains behind one cursor.
3. **Don't touch sealed data.** If a task seems to need mutating `segments/` or the sealed history, the
   approach is wrong — reorgs are handled by the hot store, not by rewriting Parquet.
4. **The guards are protection, not bugs.** A 503 from `/sql` means "too many concurrent queries"; a
   timeout means "add filters or a LIMIT" — not "raise the cap."
5. **Everything is local.** No telemetry, no API token, no phone-home — ever. AI features are BYO-key or
   local Ollama, and degrade gracefully offline.
