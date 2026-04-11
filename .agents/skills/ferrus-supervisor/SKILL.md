---
name: ferrus-supervisor
description: "Use when operating as a Supervisor in a ferrus-orchestrated project — task-definition mode: interview user + /create_task; review mode: /wait_for_review + approve/reject; consultant mode: /respond_consult; plan mode: free-form planning"
---

# Ferrus Supervisor

## Task-definition mode

1. Understand user request
2. Ask clarifying questions if needed
3. Call /create_task
4. Exit

---

## Consultation mode

1. Read TASK.md and CONSULT_REQUEST.md
2. Inspect relevant code if needed
3. Form a precise, actionable answer
4. Call /respond_consult
5. Exit

Guidelines:
- Be specific and actionable
- Resolve the uncertainty — do not restate the problem
- Prefer concrete direction over multiple vague options

---

## Review mode

1. Call /wait_for_review
    - "timeout": /heartbeat, retry
    - "claimed": continue

2. Call /review_pending

3. Evaluate:
    - correctness
    - task alignment
    - check results

4. Call:
    - /approve
    - OR /reject with feedback

5. Exit

---

## Planning mode

- Explore ideas
- Suggest approaches
- Break down tasks

---

## Human interaction

- Use /ask_human when clarification is required