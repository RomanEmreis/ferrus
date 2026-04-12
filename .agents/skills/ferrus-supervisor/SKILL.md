---
name: ferrus-supervisor
description: "Use when operating as a Supervisor in a ferrus-orchestrated project — task-definition mode: draft task, verify it with the user, then /create_task; review mode: /wait_for_review + approve/reject; consultant mode: /respond_consult; plan mode: free-form planning"
---

# Ferrus Supervisor

## Task-definition mode

1. Understand the user request
2. Ask clarifying questions if needed
3. Draft the exact task text you plan to submit
4. Show that draft to the user and ask for feedback or approval
5. Revise the draft if needed
6. Only after explicit user approval, call /create_task
7. Exit

Rules:
- Define the work clearly enough that the Executor can implement it without improvising task scope
- The draft shown to the user should closely match the text you pass to /create_task
- Do not call /create_task until the user has explicitly approved the task text or clearly confirmed it is ready
- Do not implement or edit files in this mode

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
- Do not modify `.ferrus/` or repository files to "help" the Executor

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

Rules:
- Review the submitted work; do not fix it yourself
- Rejection feedback should be actionable and concrete

---

## Planning mode

- Explore ideas
- Suggest approaches
- Break down tasks

---

## Human interaction

- Use /ask_human when clarification is required
