# Governance & scope

nuthatch is a free, AGPL-3.0 public good with a single maintainer and no direct monetization. This
document states how it is sustained, what stays out of scope regardless of who's paying, and the
neutrality guarantees that make it safe to depend on. See [RFC-0006](docs/rfcs/0006-grant-funding.md)
for the full reasoning.

## Sustainability — two independent legs

1. **Grants** (NLnet/NGI, EF ESP) fund the commons-facing roadmap — semantic layer, IVM
   generalization, GraphQL compatibility, security audit, docs. Public-benefit, milestone-based.
2. **Operator revenue-share** funds maintainer availability and operator-adjacent work (release
   engineering, guards, fleet ergonomics), proportional to hosted-service revenue.

The legs are deliberately independent: either alone sustains part-time development; nothing in the
grant plan depends on operator revenue, and vice versa. No milestone is funded by both (RFC-0006
Rule 1).

## Neutrality (the guarantee you can depend on)

**No operator has exclusivity, a private fork, partner-only features in the core, or roadmap veto.**
Any operator may host nuthatch under the AGPL; a partner's edge is partnership, being first, and
priority support of *their own* integration — never a gate on anyone else. The AGPL license makes
capture structurally impossible: anyone can run, fork, and host the exact same software.

### Operator-partnership disclosure

> An independent infrastructure operator (GraphOps) is preparing a hosted offering of nuthatch under
> the AGPL and shares revenue with the maintainer to fund core development. The relationship is
> partnership, not ownership: no exclusivity, no relicensing, no private features, no roadmap veto.

_(Terms are summarised here as existence + shape once agreed; amounts stay private — RFC-0006
Acceptance.)_

## The dividing line: core vs operator layer

nuthatch ships the **guards and signals** that make it safe to run as a service; it does **not** grow
the operator's product into the binary. Concretely (RFC-0005 §6):

| In the core (nuthatch) | The operator's layer (a gateway in front) |
|---|---|
| `/sql` resource guards (timeout, row cap, concurrency) — bound *how much* | Authentication — decides *who* may query |
| `/metrics`, `/health`, structured logs | Metering, billing, quotas |
| Bind posture + a loud warning off-localhost | Multi-tenancy, per-tenant isolation |
| Config/data stability contract | The hosted product itself |

## What we will not do — for funding or partnership

Non-negotiable regardless of who asks or pays:

- **No token**, no decentralised-network features, no staking.
- **No telemetry / phone-home**; no mandatory API tokens or gated data services in the data path.
- **No relicensing** of the AGPL core; **no private forks**; **no partner-only features** in core.
- **No roadmap veto** for any funder or partner (input is welcome; veto is not).
- **No auth / metering / multi-tenancy in core** — that is the operator layer, and it is contractual.

If a funder or partner requires any item on this list, we decline that term rather than the principle.

## Release integrity & key custody

Releases are the supply chain an operator depends on. Signing/release-key custody, and a named
successor/escrow arrangement for those keys, are tracked as an open governance item (RFC-0006 Q3) to
be settled while the project is small. Until then: releases are cut from tagged commits on `main`,
published to GitHub Releases (with per-artifact SHA-256) and crates.io, and reproducible from the
pinned toolchain.
