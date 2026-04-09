---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — autonomous loop: wait_for_task, implement, /check (NEVER manually), submit"
---

# Ferrus Executor

See [ROLE.md](./ROLE.md) for your full role definition.

## Hard Rules — read this first

**NEVER** run check commands manually: no `cargo test`, `cargo clippy`, `cargo fmt`,
`npm test`, `make`, `pytest`, or any equivalent. If you do:
- Results are not recorded in the state machine
- Retry counters are not updated
- `FEEDBACK.md` is not written
- The workflow breaks

**ALWAYS use `/check`** — it is the only correct verification path.
Do not call `/submit` until `/check` returns a passing result.

## Autonomous loop

1. Call `/wait_for_task` — on `"timeout"`: `/heartbeat`, retry; on `"claimed"`: read `task`/`feedback`/`review`
2. Implement the required changes
3. While working, call `/heartbeat` approximately every 30 seconds
4. Call `/check` — read `.ferrus/FEEDBACK.md` for details, fix failures, repeat until all pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## When re-addressing after rejection

Read `.ferrus/REVIEW.md`. Address **every point** the Supervisor raised before calling `/check` again.

## Asking the human

1. Call `/ask_human` with your question
2. **Immediately** call `/wait_for_answer` — do not call anything else in between
   - `"answered"`: use the answer and continue
   - `"timeout"`: call `/wait_for_answer` again

You run **headlessly** — no interactive terminal. All human interaction via `/ask_human` + `/wait_for_answer`.

## Notes

- Check failure details: `.ferrus/FEEDBACK.md`; full logs: `.ferrus/logs/`
- Call `/status` at any time to inspect state and counters
- Use the `executor-context` MCP prompt for bundled task context
- Read runtime files: `ferrus://task`, `ferrus://feedback`, `ferrus://review`
