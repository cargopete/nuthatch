# Authored views — a nest's logic layer (RFC-0018 §1)

A nest can ship **authored SQL views** in `views/*.sql` — the derivations it exists to answer ("top
holders", "daily volume", "delegator exposure"). This is what makes a nest feel alive: its reasoning
is visible, diffable, and agent-legible instead of living in ad-hoc REPL queries that get lost.

## What a view is (and isn't)

- A view is a read-only `CREATE VIEW …` over the nest's tables. It's **recomputed per query** over the
  live tip ∪ sealed history (one surface) — never materialised, never written back, never in the
  ingest/decode/seal path. Adding a view can't corrupt data or slow indexing.
- It is **not** an incrementally-maintained entity (that's the deferred §3 / the built-in `balances`
  IVM view). A view is just SQL that runs when you query it.

## Adding one

Drop a file in `views/`. Files load in **sorted filename order** (`10-…`, `20-…`), so a `20-` view can
build on a `10-` one.

```sql
-- views/10-top-recipients.sql
CREATE VIEW top_recipients AS
  SELECT "to" AS addr, count(*) AS n
  FROM "usdc__transfer"
  GROUP BY "to"
  ORDER BY n DESC
  LIMIT 20;
```

Then **describe what it means** in `semantic.toml` so the MCP and `/schema` render it:

```toml
[view.top_recipients]
description = "The 20 addresses that received the most USDC transfers."
```

Query it by name — it's just another relation:

```sh
nuthatch sql "SELECT * FROM top_recipients"
```

## The footguns (the same three every agent trips on)

1. **Reserved-word columns.** `from` and `to` are SQL keywords — double-quote them: `SELECT "from"`.
   Call the MCP `schema` tool; it lists exactly which columns are reserved for this nest.
2. **Big-int columns are exact text.** A `uint256`/`int256` column (e.g. `value`) is stored as a text
   string. Never `SUM(value)` — use its derived `value_dec` companion: `SUM(value_dec)`. `schema`
   lists which columns have a `_dec`.
3. **The hot/cold seam.** A view sees the whole hot ∪ cold surface, so results are current — but it's
   recomputed each query, not a maintained snapshot. `schema` shows `sealed_through` vs the tip.

## Validation is loud (RFC-0018 §1)

A broken or **drifted** view — one that references a table or column the ABI no longer produces —
fails `nuthatch check` with a fuzzy-matched fix hint, and warns loudly at `dev` startup. It never
silently vanishes. A bad view is fault-isolated: it never disables the other views or the query
surface, but you'll always hear about it. So: write the view, run `nuthatch check`, fix what it tells
you.
