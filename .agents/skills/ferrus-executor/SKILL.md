---
name: ferrus-executor
description: "Use when operating as an Executor in a ferrus-orchestrated project — autonomous loop: wait_for_task, implement, /check (NEVER manually), submit"
---

# Ferrus Executor

## Autonomous loop

1. Call /wait_for_task
   - "timeout": call /heartbeat, retry
   - "claimed": read task/feedback/review

2. Understand the task
   - read TASK.md
   - inspect relevant files

3. Implement
   - make minimal, correct changes

4. Maintain lease
   - call /heartbeat ~ every 30 seconds

5. Verify
   - call /check
   - if checks fail: read FEEDBACK.md, fix issues, repeat
   - if checks pass: immediately call /submit

6. Submit
   - include:
      - summary
      - verification steps
      - limitations

7. Return to step 1

---

## After rejection

- Read REVIEW.md
- Address ALL points
- Then run /check again

---

## Human interaction

1. Call /ask_human
2. Immediately call /wait_for_answer
   - "answered": continue
   - "timeout": retry

---

## Completion invariant

Never stop after a successful /check.
Your next action must be /submit.

---

## Useful resources

- ferrus://task
- ferrus://feedback
- ferrus://review

---

## Notes

- Logs: `.ferrus/logs/`
- Status: `/status`
