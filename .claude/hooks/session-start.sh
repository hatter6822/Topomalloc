#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# SessionStart hook (W0-9): make a Claude-on-web / CI-agent session build-ready
# with no manual steps. It ensures the pinned Rust toolchain, components, and
# cross targets are present (via `cargo xtask setup`, the same entry point devs
# use) and prints a one-line readiness summary. Fast, idempotent, non-interactive.
#
# `cargo xtask setup` also provisions the pinned Lean toolchain via
# `scripts/setup_lean.sh` (best-effort, non-fatal): it downloads from the
# reachable GitHub releases — not elan's release host — self-installs `zstd` if
# missing, and puts `lake`/`lean` on PATH (persisting via $CLAUDE_ENV_FILE /
# $GITHUB_PATH) so the formal-model gates run in-session. If Lean cannot be
# provisioned (no network/privileges), `xtask` skips the Lean steps cleanly and
# the session can still run the rest of `cargo xtask ci`.
set -uo pipefail

# Only provision in the remote (web / CI-agent) environment; a local checkout
# already has whatever the developer installed.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

if command -v cargo >/dev/null 2>&1; then
  # `rust-toolchain.toml` pins the exact toolchain (auto-installed on first use);
  # `xtask setup` adds the x86-64 + AArch64 targets and rustfmt/clippy.
  cargo run -q -p xtask -- setup \
    || echo "topomalloc: 'xtask setup' reported issues; continuing (CI re-provisions)."
else
  echo "topomalloc: cargo not found — Rust toolchain unavailable in this session." >&2
fi

echo "topomalloc: session ready — run 'cargo xtask ci' to build, lint, and test."
