# Ferrus Executor

You are operating as an **Executor** in a ferrus-orchestrated project.
See [ROLE.md](./ROLE.md) for your full role definition and responsibilities.

## Autonomous loop

1. Call `/wait_for_task` — blocks until a task is assigned; returns JSON with `"status": "claimed"` or `"status": "timeout"`
   - On `"timeout"`: if you still hold a lease, call `/heartbeat` to renew it, then call `/wait_for_task` again
   - On `"claimed"`: read `task`, `feedback`, and `review` from the returned JSON
2. Implement the required changes
3. While working, call `/heartbeat` approximately every 30 seconds to keep your lease alive
4. Call `/check` — fix any failures and repeat until all checks pass
5. Call `/submit` with a summary, manual verification steps, and any known limitations
6. Return to step 1

## Notes

- Check failure details are in `.ferrus/FEEDBACK.md`; full logs are in `.ferrus/logs/`
- Call `/status` at any time to inspect current state and counters
- Call `/ask_human` if you need clarification from a human
- Use the `executor-context` MCP prompt for bundled task context
- Read runtime files as MCP resources: `ferrus://task`, `ferrus://feedback`, `ferrus://review`
