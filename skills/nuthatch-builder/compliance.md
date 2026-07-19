# nuthatch compliance pack

Only relevant when the user asks for screening / flags / exposure / an audit trail (RFC-0008). Amounts
throughout are i128 base units, returned as decimal strings. Flags are authoritative in
[cli-reference.md](cli-reference.md).

## The pieces, and when each applies

| Feature | What it does | Drive it with |
|---|---|---|
| **Labels** | Content-addressed sets of tagged addresses (the annotation substrate). | `nuthatch labels import <file>` / `labels list` |
| **Lists** | Sanctions/watch lists as content-addressed snapshots (fetched out-of-band). | `nuthatch lists fetch --list <name>` / `lists list` |
| **Screen** | Screen sealed transfers against a list snapshot; records `sanction_hit` annotations. Replayable: same list hash + range → identical hits. | `nuthatch screen --list <hash> --from <b> --to <b>` |
| **Flags** | `threshold` (single transfer ≥ N) and `velocity` (windowed volume) flags. Configured in `nuthatch.toml` `[flags]`. | query the `flags` MCP tool / `/flags` |
| **Exposure** | An address's direct counterparty exposure to the labeled set. | the `exposure` MCP tool / `/exposure/{addr}` |
| **Pack** | Build/sign/verify the compliance-pack manifest (ed25519). | `nuthatch pack keygen` → `pack build --key …` → `pack verify` |
| **Audit** | `replay` re-proves the recorded annotations; `report` summarises a block range. | `nuthatch audit replay --from … --to …` / `audit report … --json` |

## The one property that matters

Every compliance annotation is **replayable**: `nuthatch audit replay` re-runs screening over the
sealed segments and confirms the stored hits reproduce *exactly*. A hit carries its `list_snapshot`
hash, so it traces to (list version, block, component). This is verifiability by deterministic
re-execution — nothing heavier (no TEE, no zk).

```sh
# Screen a range against a fetched sanctions snapshot, then prove it reproduces.
nuthatch lists fetch --list ofac-sdn
nuthatch screen --list <hash> --from 18000000 --to 18100000
nuthatch audit replay --from 18000000 --to 18100000     # must reproduce byte-for-byte
```

The `sanction_hit` table is queryable SQL:

```sh
nuthatch sql "SELECT block_number, counterparty, list_snapshot FROM sanction_hit WHERE lower(address) = '0x…'"
```

Do not present compliance output as legal/regulatory advice — it is a deterministic, auditable
annotation layer, not a compliance opinion.
