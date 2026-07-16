---
name: Bug report
about: Something the indexer did that it shouldn't have
title: ""
labels: bug
assignees: ""
---

**Not a security issue?** If this is a vulnerability (guard bypass, sandbox escape, segment
corruption), stop and follow [SECURITY.md](../../SECURITY.md) instead — please don't file it in
the open.

## What happened

A clear description of the wrong behaviour.

## What you expected

## Reproduction

The exact commands, in order. The more of this that's copy-pasteable, the faster it's fixed.

```
nuthatch init 0x... --chain mainnet
nuthatch dev ...
```

- Chain(s):
- Contract address(es), if relevant:
- Did it happen on the first run, or after a restart / reorg?

## Environment

- `nuthatch --version`:
- OS + arch (e.g. macOS aarch64, Linux x86_64):
- Installed via: curl script / crates.io / built from source (commit)
- RPC endpoint type: public / own node / archive

## Logs

Relevant log output, the tail of `/metrics` if the process is still up, and the panic backtrace
if it panicked (`RUST_BACKTRACE=1`). Redact anything private — including any API key baked into
an RPC URL.
