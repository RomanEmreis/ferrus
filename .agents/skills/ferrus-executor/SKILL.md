---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — single-session flow: wait_for_task, implement, /check (NEVER manually), submit"
---

# Ferrus Executor

## Session lifecycle

Each Executor session is a single worker pass:

1. Call `/wait_for_task` first
   - `"claimed"`: use the returned task / feedback / review context
   - `"timeout"`: retry only while the reported state is `Executing`, `Fixing`, or `Addressing`

2. Understand the task
   - inspect the relevant repository files
   - use `TASK.md`, `FEEDBACK.md`, and `REVIEW.md` only as supporting context, not as a substitute for Ferrus tool results

3. Implement
   - make the smallest correct change set that satisfies the task

4. Maintain the lease
   - call `/heartbeat` roughly every 30 seconds while you hold the task

5. Escalate when blocked
   - use `/consult`, then immediately `/wait_for_consult`, for technical or architectural uncertainty
   - before `/consult`, read `ferrus://consult_template` and format the request with that template exactly
   - do not use `/consult` for Ferrus tool availability, MCP failures, or workflow mechanics; if a required Ferrus tool fails or is cancelled, retry that same tool
   - if repeated Ferrus tool retries do not unblock you, and `/consult` is not the right path or did not resolve the blocker, use `/ask_human` instead of stalling
   - use `/ask_human`, then immediately `/wait_for_answer`, for missing requirements, decisions a human must make, or a real execution dead end that cannot be resolved via tool retry or `/consult`

6. Verify
   - call `/check`
   - if checks fail: read `FEEDBACK.md`, fix the issues, and call `/check` again
   - if checks pass: immediately call `/submit`

7. Submit and stop
   - `/submit` must include summary, manual verification steps, and known limitations when relevant
   - after `/submit`, this Executor session is done
   - if review is rejected, HQ will start a fresh Executor session, and that new session must begin again with `/wait_for_task`

## Hard rules

- `/wait_for_task` is the required first step for a new Executor session
- `/check` is the only valid verification mechanism; never run tests, builds, or linters manually
- a green `/check` is not completion; the next action must be `/submit`
- `/consult` is only for code/task/architecture uncertainty, not for asking what to do about missing Ferrus tools or workflow rules
- `/ask_human` is the last-resort escape hatch when you are genuinely stuck; use it instead of looping or stalling
- do not emulate Ferrus tools by editing `.ferrus/` files or manually advancing `STATE.json`
- if a required Ferrus MCP tool is cancelled or unavailable, retry that tool; do not invent an on-disk fallback for task claiming, checking, or submitting

## After rejection

- the rejection is delivered to the next Executor session via `/wait_for_task`
- address every point in `REVIEW.md`
- rerun `/check`, then `/submit`

## Useful resources

- `ferrus://task`
- `ferrus://feedback`
- `ferrus://review`
- `ferrus://consult_template`
