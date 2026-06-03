<!-- SPDX-License-Identifier: MIT -->
# `.claude/` — Claude Code on the web configuration

Makes a Claude-on-web / CI-agent session build-ready with no manual steps (W0-9).

| Item | Charter |
|------|---------|
| `hooks/session-start.sh` | SessionStart hook: runs `cargo xtask setup` (pinned Rust toolchain + x86-64/AArch64 cross targets + the Lean toolchain via `scripts/setup_lean.sh`) and prints a one-line readiness summary. Fast, idempotent, and a no-op outside the remote environment (`$CLAUDE_CODE_REMOTE`). |
| `settings.json` | Registers the SessionStart hook. |

Once merged to the default branch, all future web sessions use the hook, so an
agent can run `cargo xtask ci` immediately. See the SessionStart-hook docs at
<https://code.claude.com/docs/en/claude-code-on-the-web>.
