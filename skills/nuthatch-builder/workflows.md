# nuthatch workflows

Copy-paste-runnable recipes. Flags are authoritative in [cli-reference.md](cli-reference.md); config
keys in [config-reference.md](config-reference.md).

## Index one contract (the happy path)

```sh
nuthatch init 0xA0b86991c6218b36c1D19D4a2e9Eb0cE3606eB48   # chain auto-detected
nuthatch dev
nuthatch sql "SELECT count(*) FROM usdc__transfer"
```

If auto-detect can't find the contract (or you want a specific chain), pass `--chain mainnet |
arbitrum-one | base`. To index only recent history instead of from deployment: `nuthatch dev
--backfill 100000` (last 100k blocks). Point at your own node with `--rpc https://…` to dodge
public-RPC limits.

## Index several contracts together

Pass multiple addresses to `init`, or grow an existing nest with `add` (no re-init):

```sh
nuthatch init 0xUSDC 0xWETH --alias usdc,weth
# …later…
nuthatch add 0xDAI --alias dai        # resolves the ABI, appends to nuthatch.toml, regenerates artifacts
```

`add` never re-detects the chain (a nest is one chain) and refuses an address already present. The next
`dev` backfills the new contract from its own deployment block; the existing ones resume from their
cursor.

## Fast backfill of a long history

For a from-deployment backfill, seal finalized history straight to Parquet (skip the hot store) and
overlap RPC latency:

```sh
nuthatch dev --seal-direct --concurrency 8
```

Keep `--concurrency` at 1 against a single public endpoint (high concurrency to one host can stall the
runtime); use 8–16 only when you have multiple `rpc_urls` or your own node. For a *sparse* contract over
a long range, widen the getLogs window: `--window 50000`.

## Factories / dynamic contracts

When a factory deploys children (e.g. a pool factory), declare a template + factory in `nuthatch.toml`
(see config-reference). Children are discovered at runtime and indexed into shared `{template}__*`
tables, distinguished by the `address` column. Each template gets a `{template}__children` view:

```sh
nuthatch sql "SELECT address, discovered_block, parent_address FROM \"pool__children\""
```

No redeploy per child; the discovery is deterministic and survives restarts (rebuilt from stored
factory events).

## Publish and consume a nest (the deploy unit)

A nest is a self-contained, content-addressed deploy unit — ABIs vendored, config + semantics pinned.

```sh
nuthatch nest bundle .                     # → a <name>-<hash>.bundle file; prints the content-address
nuthatch nest load ./my-nest.bundle        # verifies every file hash + re-derives the decode registry
# load also accepts an http(s):// URL to a .bundle, so a nest is shareable from any static host:
nuthatch nest load https://host/my-nest.bundle --expect <hash>
```

`load` fails loudly if the inputs don't reproduce the manifest's registry hash — same inputs + same
generator → same decode, verifiably. `semantic.toml` travels in the bundle and is hash-checked too.

You can also `init --from <git-url|dir>` to start from a published nest instead of an address.

## Run many nests in one process (a roost)

A *roost* hosts several nests **on the same chain** behind one API, sharing one cursor and one getLogs
per window — N nests for roughly one nest's RPC cost. Create a `roost.toml` (see config-reference) and:

```sh
nuthatch roost dev --dir .            # serves /nests + each nest under /<name>/…
```

Each nest is isolated (own store, own reorg blast radius) but shares the finality view. A per-runtime
RAM budget (`max_rss_mb`, default 2048) refuses a mount projected to blow it.

## Wire an AI client to a running nest

```sh
nuthatch dev &                        # the index the agent queries
nuthatch mcp --print-config           # copy-paste MCP client config (Claude Code / any client)
# or, Claude Code directly:
claude mcp add nuthatch -- nuthatch mcp --url http://127.0.0.1:8288
```

The agent then calls the `schema` tool (per-nest, with footguns + coverage), writes SQL, and gets
compact results with a provenance stamp. Fully offline; nothing phones home.

## Run it in production

`dev` is the serve command. Put it under systemd or Docker and a reverse proxy — copy-paste recipes are
in [`docs/operators.md`](https://github.com/nuthatch-indexer/nuthatch/blob/main/docs/operators.md).
Off-localhost, set `NUTHATCH_ADMIN_TOKEN` and bind behind a gateway (the `/sql` guards bound *how much*,
never *who* — auth is the operator's layer).
