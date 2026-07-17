#!/usr/bin/env bash
#
# try-nuthatch.sh — install & smoke-test the Nuthatch indexer, capturing every
# failure mode so it can be diagnosed afterwards.
#
# Nuthatch (https://github.com/nuthatch-indexer/nuthatch) is a self-hosted-first,
# AI-native EVM blockchain indexer in one Rust binary. This script installs it,
# scaffolds the smallest possible nest (a single contract — USDC on mainnet),
# runs `nuthatch dev` just long enough to prove the local API answers, then shuts
# it down. Everything is teed to a log for post-mortem.
#
# Nothing here is destructive: it installs to ~/.local/bin and works inside a
# throwaway ./nuthatch-trial/ directory. It indexes over public RPC, so no API
# key is required.
#
# Requirements: an Apple-Silicon Mac or Linux x86_64 (prebuilt binary targets).
# On other platforms the installer will tell you to `cargo install nuthatch`.
#
# Usage:  ./scripts/try-nuthatch.sh                 # full run
#         KEEP_RUNNING=1 ./scripts/try-nuthatch.sh  # leave `dev` running afterwards
#         WORKDIR=/path DEV_BOOT_TIMEOUT=90 ./scripts/try-nuthatch.sh
#
set -uo pipefail   # deliberately NOT -e: we want to catch failures, not die on them

# ---- config -----------------------------------------------------------------
CONTRACT="0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"   # USDC on mainnet, smallest nest
CHAIN="mainnet"
API="http://127.0.0.1:8288"
WORKDIR="${WORKDIR:-$(pwd)/nuthatch-trial}"
LOG="${WORKDIR}/trial.log"
DEV_LOG="${WORKDIR}/dev.log"
INSTALL_DIR="${NUTHATCH_INSTALL_DIR:-$HOME/.local/bin}"
DEV_BOOT_TIMEOUT="${DEV_BOOT_TIMEOUT:-60}"   # seconds to wait for the API to answer

mkdir -p "$WORKDIR"
: > "$LOG"

# ---- helpers ----------------------------------------------------------------
say()  { printf '\n\033[1;36m==>\033[0m %s\n' "$*" | tee -a "$LOG"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n'  "$*" | tee -a "$LOG"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n'  "$*" | tee -a "$LOG"; }
die()  { printf '\033[1;31m FAIL\033[0m %s\n' "$*" | tee -a "$LOG"; exit 1; }

# run a command, tee its combined output into the log, return its real exit code
run() {
  say "\$ $*"
  { "$@" 2>&1; echo "__exit=$?"; } | tee -a "$LOG" | sed 's/^/    /'
  return "${PIPESTATUS[0]}"
}

# ---- 0. sanity: architecture ------------------------------------------------
say "Checking platform"
ARCH="$(uname -m)"
OS="$(uname -s)"
if [[ "$OS" == "Darwin" && "$ARCH" != "arm64" ]]; then
  warn "macOS on '$ARCH', not arm64 — there's no prebuilt binary for Intel Macs."
  warn "The installer will error; build from source instead: cargo install nuthatch (Rust 1.95+)."
else
  ok "$OS / $ARCH"
fi

# ---- 1. install (idempotent) ------------------------------------------------
if command -v nuthatch >/dev/null 2>&1; then
  ok "nuthatch already on PATH at $(command -v nuthatch)"
else
  say "Installing via the official one-liner"
  run bash -c 'curl -fsSL https://nuthatch-indexer.com/install.sh | sh' \
    || die "installer failed — see $LOG (network? release asset missing? checksum mismatch?)"
fi

# ---- 2. PATH resolution -----------------------------------------------------
if ! command -v nuthatch >/dev/null 2>&1; then
  if [[ -x "$INSTALL_DIR/nuthatch" ]]; then
    warn "Installed to $INSTALL_DIR but that's not on PATH. Adding it for this run."
    warn "Persist it with:  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc"
    export PATH="$INSTALL_DIR:$PATH"
  else
    die "nuthatch not found on PATH nor at $INSTALL_DIR/nuthatch after install."
  fi
fi
BIN="$(command -v nuthatch)"
ok "Using binary: $BIN"

# ---- 3. macOS Gatekeeper / quarantine ---------------------------------------
if [[ "$OS" == "Darwin" ]] && xattr "$BIN" 2>/dev/null | grep -q 'com.apple.quarantine'; then
  warn "Binary is quarantined by Gatekeeper — clearing the flag."
  run xattr -d com.apple.quarantine "$BIN"
fi

# ---- 4. version smoke test --------------------------------------------------
run nuthatch --version || warn "'--version' failed; the binary may be blocked or corrupt."

# ---- 5. scaffold the nest ---------------------------------------------------
say "Scaffolding a single-contract nest in $WORKDIR"
cd "$WORKDIR" || die "cannot cd into $WORKDIR"
run nuthatch init "$CONTRACT" --chain "$CHAIN" \
  || die "'nuthatch init' failed — ABI resolution (Sourcify/Etherscan) or RPC reachability?"
[[ -f nuthatch.toml ]] && ok "nuthatch.toml created" || warn "no nuthatch.toml — init may not have completed."

# ---- 6. run `dev` in the background, wait for the API -----------------------
say "Starting 'nuthatch dev' (backgrounded; logging to $DEV_LOG)"
nuthatch dev >"$DEV_LOG" 2>&1 &
DEV_PID=$!
ok "dev PID = $DEV_PID"

cleanup() {
  if [[ "${KEEP_RUNNING:-0}" == "1" ]]; then
    warn "KEEP_RUNNING=1 — leaving dev running (PID $DEV_PID). Kill it with: kill $DEV_PID"
  elif kill -0 "$DEV_PID" 2>/dev/null; then
    say "Stopping dev (PID $DEV_PID)"
    kill "$DEV_PID" 2>/dev/null
    wait "$DEV_PID" 2>/dev/null
  fi
}
trap cleanup EXIT INT TERM

say "Waiting up to ${DEV_BOOT_TIMEOUT}s for the API at $API/tables"
BOOTED=0
for ((i=1; i<=DEV_BOOT_TIMEOUT; i++)); do
  if ! kill -0 "$DEV_PID" 2>/dev/null; then
    warn "dev process exited early. Last lines of $DEV_LOG:"
    tail -n 30 "$DEV_LOG" | tee -a "$LOG" | sed 's/^/    /'
    die "'nuthatch dev' crashed on boot — see above (bad RPC? port 8288 in use? redb error?)."
  fi
  if curl -fsS "$API/tables" >/dev/null 2>&1; then
    BOOTED=1; ok "API answered after ${i}s"; break
  fi
  sleep 1
done
[[ "$BOOTED" == "1" ]] || { tail -n 30 "$DEV_LOG" | tee -a "$LOG" | sed 's/^/    /'; die "API never came up within ${DEV_BOOT_TIMEOUT}s."; }

# ---- 7. probe the endpoints -------------------------------------------------
say "Probing endpoints"
for ep in /tables /metrics; do
  if curl -fsS "$API$ep" >/dev/null 2>&1; then ok "GET $ep -> 200"; else warn "GET $ep failed"; fi
done
say "Sample: GET /tables"
curl -fsS "$API/tables" 2>&1 | tee -a "$LOG" | sed 's/^/    /'

# ---- 8. verdict -------------------------------------------------------------
say "Done."
ok "Full transcript: $LOG"
ok "dev server log:  $DEV_LOG"
if grep -qiE 'error|panic|fatal|failed' "$DEV_LOG" 2>/dev/null; then
  warn "The dev log contains error-ish lines — worth a read:"
  grep -inE 'error|panic|fatal|failed' "$DEV_LOG" | head -n 20 | sed 's/^/    /'
fi
