# Security policy

nuthatch is a single-binary indexer that people run on their own machines and, increasingly,
as a hosted service in front of untrusted callers. Two attack surfaces matter most:

1. **The `/sql` and MCP surfaces** — a caller can run arbitrary read-only SQL. The core ships
   resource guards (timeout, row cap, concurrency semaphore) and binds to `127.0.0.1` by
   default with a loud warning off-localhost. Authentication is deliberately *not* in the core
   (see [GOVERNANCE.md](GOVERNANCE.md) — that is the operator layer). A report that defeats the
   guards, reads outside the attached read-only segments, or escalates a read into a write is
   in scope.
2. **The WASM transform host boundary** — components are sandboxed by capability injection at
   composition time; a zero-capability component must be inert. A report that lets a component
   reach a capability it was not granted (filesystem, network, host memory) is in scope, and is
   the surface we most want audited (RFC-0006 M4).

Also in scope: reorg / finality handling that could corrupt sealed segments, decode paths that
panic or mis-attribute on adversarial ABIs/logs, and any path that breaks the ≤2 GB footprint
budget into an OOM DoS.

Out of scope: third-party RPC endpoints you point nuthatch at (trust them or don't), and
anything requiring a malicious operator who already controls the host.

## Reporting

**Please do not open a public issue for a vulnerability.** Use GitHub's private advisory flow:
**Security → Report a vulnerability** on the repo, or email the maintainer at the address on
the [GitHub profile](https://github.com/cargopete). Include a description, affected version
(`nuthatch --version`), and a reproduction if you have one.

Expect an acknowledgement within a few days — this is a solo-maintained public good, so please
be patient with the "few," not the "days." We will agree a disclosure timeline with you; the
default is coordinated disclosure once a fix ships, with credit unless you'd rather stay
anonymous.

## Supported versions

Pre-1.0, only the latest release (currently the `0.1.x` line) receives security fixes. Releases
are cut from tagged commits on `main`, published to GitHub Releases with per-artifact SHA-256
and to crates.io, and reproducible from the pinned toolchain (see
[GOVERNANCE.md § Release integrity](GOVERNANCE.md#release-integrity--key-custody)).
