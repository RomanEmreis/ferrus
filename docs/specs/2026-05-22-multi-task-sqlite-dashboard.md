# Multi-task SQLite Runtime and HQ Dashboard Specification

## Summary

This specification captures the current design direction and implementation status for moving `ferrus`
from a single active task model toward a SQLite-backed, multi-task orchestration runtime with a new HQ dashboard.

The core boundary is:

- Markdown stores human-readable intent, specifications, task artifacts, and review/submission artifacts.
- SQLite stores coordination, recovery, runtime state, task/run/event records, leases, retries, and process/session metadata.

## Goals

- Keep `.ferrus/` in the repository as the human-readable project surface.
- Move machine-local runtime state into `~/.ferrus/projects/<project-id>/`.
- Support multiple queued or active tasks without using project names as identity keys.
- Make crash recovery deterministic from SQLite runtime state.
- Preserve the current single-task Supervisor-Executor flow while adding multi-task primitives.
- Add an HQ dashboard that shows project state, milestones, command output, errors, and human questions without relying on append-only terminal logs.
- Add a `/run` flow that can prepare and start multiple ready tasks deterministically.

## Non-Goals

- Publishing a new stable release before the multi-task flow and dashboard are stable.
- Removing `STATE.json` immediately before the SQLite-backed replacement is complete.
- Starting true parallel code changes without worktree isolation and integration policy.
- Asking agents to decide which milestones are ready without deterministic HQ validation.

## Context

The previous runtime model had one current task and stored most active state in `.ferrus/STATE.json`.
That was enough for the first Supervisor-Executor loop, but it is weak for parallel work, history,
lease recovery, and querying task/run state.

The new direction separates project-local artifacts from machine-local runtime state:

```text
repo/
  .ferrus/
    SPEC.md
    TASK.md
    project.toml
    tasks/
      t-001.md
      t-002.md
    runs/
      t-001/
        REVIEW.md
        SUBMISSION.md
        logs/

~/.ferrus/
  projects/
    <project-id>/
      project.toml
      ferrus.db
      logs/
```

The project id is stable and opaque. Project name is metadata only because names can collide or change.

## Current Decisions

- `~/.ferrus/projects/<project-id>/project.toml` is the machine-local project registry entry.
- `repo/.ferrus/project.toml` is a local reference to the project id and data dir.
- `ferrus.db` is the runtime coordination store.
- `TASK.md` is now a task template, not the active task source of truth.
- Numbered task artifacts live under `.ferrus/tasks/t-00x.md`.
- Run artifacts live under `.ferrus/runs/<task-id>/`.
- Task ids are monotonic within the project. Completed tasks are not reused.
- `/task` remains the single-task command.
- `/task --manual` remains the ad-hoc task path without spec/milestone context.
- `/run` will become the batch scheduler command for ready milestones.
- `/run --limit N` means "start at most N ready tasks", subject to deterministic availability checks.
- If the user requests more tasks than are ready, HQ asks whether to continue with the available count.
- HQ determines which milestones are ready before launching a supervisor.
- The supervisor drafts task artifacts for an exact set of milestones chosen by HQ.
- Agents should not be trusted to infer task identity. Runtime should pass task identity deterministically.
- MCP resource `ferrus://task/<task-id>` is supported for numbered task artifacts.
- MCP resource `ferrus://task_template` returns the task template.
- A new MCP tool should enqueue task artifacts without entering the old single active task state.

## Proposed MCP Tool

Use `/enqueue_task`.

Semantics:

- allowed for supervisor task preparation flows;
- writes one numbered task artifact under `.ferrus/tasks/`;
- records a SQLite task row with status `pending`;
- stores origin metadata such as `spec_path` and `milestone_id` when available;
- does not set `STATE.json` active task fields;
- does not start an executor directly;
- requires explicit user approval before the supervisor calls it.

This keeps `/create_task` focused on the old single-task path and avoids overloading it with queue semantics.

## Milestone Readiness

Milestone display and `/run` should share the same deterministic readiness calculation.

Statuses:

- `done`: the milestone is checked as `[x]`.
- `ready`: the milestone is unchecked and all dependencies are complete or `none`.
- `pending`: the milestone is unchecked and at least one dependency is not complete.

For scheduler eligibility, readiness is not enough. `/run` must also exclude milestones that already have
an enqueued, running, addressing, reviewing, or otherwise active task for the same `(spec_path, milestone_id)`.

The shared readiness function should live close to spec parsing, most likely in `src/specs.rs`, and return enough
detail for both UI and scheduler decisions.

## Runtime Schema Direction

The current SQLite runtime tables are:

```text
tasks:
  id
  path
  status

runs:
  id
  task_id
  role
  agent
  status
  started_at
  updated_at
  pid
  workspace_path

events:
  id
  run_id
  type
  payload_json
  created_at
```

The task table has already grown additional runtime fields for leases, retry/review counters, pauses,
and failure reasons.

For `/run` and duplicate prevention, tasks should also store:

- `spec_path`
- `milestone_id`
- optionally `title` or a compact display label

## Crash Recovery

Recovery is SQLite-first:

- read `ferrus.db`;
- find runs where status is active, such as `running`, `checking`, or `reviewing`;
- verify pid/process/session state;
- mark dead processes as `interrupted`;
- restore the next step when artifacts exist;
- release stale task locks or leases when they expired;
- keep Markdown artifacts as durable human-readable records.

## What Is Already Implemented

- Global project directory under `~/.ferrus/projects/<project-id>/`.
- Project-local `.ferrus/project.toml` reference.
- Runtime database `ferrus.db`.
- SQLite tables for tasks, runs, and events.
- Task runtime fields for leases, heartbeat, pause status, retry counters, review counters, and failure reasons.
- Task origin metadata for `spec_path` and `milestone_id`.
- `ferrus migrate`, `ferrus doctor`, `ferrus recover`, `ferrus projects list`.
- Runtime inspection commands for tasks, runs, and events.
- Numbered task artifacts under `.ferrus/tasks/`.
- Run artifact directories under `.ferrus/runs/<task-id>/`.
- `TASK.md` as a reusable task template.
- MCP resources `ferrus://task`, `ferrus://task/<task-id>`, and `ferrus://task_template`.
- MCP tool `/enqueue_task` for queued pending task artifacts.
- Lease claiming, lease renewal, stale lease recovery, and DB lease handoff fixes.
- Migration fixes for active artifacts and Windows path handling.
- Completed task history preservation during HQ reset paths.
- Supervisor/executor launch preflight validation in backend-specific agent modules.
- New HQ dashboard foundation with project panel, milestones panel, command output area, prompt/footer, error display, and ask-human groundwork.
- Dashboard fixes for prompt stability, multiline input, command output spacing, stderr/error display, and backend-specific preflight errors.

## What Remains

- Add shared milestone readiness calculation with `ready`, `pending`, and `done`.
- Update dashboard milestone rendering to use shared readiness, not only `[x]`.
- Add `spec_path` and `milestone_id` metadata to SQLite task records.
- Add duplicate prevention for `(spec_path, milestone_id)` tasks.
- Add `/enqueue_task` MCP tool.
- Add supervisor prompt for batch task preparation that requires exactly the HQ-selected milestone set.
- Add `/run` command parsing and deterministic ready milestone selection.
- Add `/run --limit N` confirmation when fewer than N milestones are eligible.
- Connect `/run` to one interactive supervisor session that prepares multiple approved task artifacts.
- Start executors for queued tasks within the configured parallelism limit.
- Add worktree isolation via `runs.workspace_path`.
- Add final integration/review policy for parallel outputs.
- Move remaining runtime state out of `STATE.json` once SQLite can fully replace it.
- Add multi-task ask-human queue handling.
- Connect dashboard panels to real queued/running/reviewing task and run state.
- Pass task id and agent id deterministically into agent MCP runtime context.

## Milestones

### [x] #1.0 Project registry and SQLite runtime foundation

ID: m1.0
Depends on: none

Create the global project registry, local project reference, runtime database, task/run/event tables,
and migration/doctor/recover commands.

### [x] #1.1 Numbered task and run artifacts

ID: m1.1
Depends on: m1.0

Move task history into numbered task artifacts and run-specific artifact directories while preserving
legacy compatibility where needed.

### [x] #1.2 Lease and recovery hardening

ID: m1.2
Depends on: m1.0

Make task claiming, lease renewal, handoff statuses, stale lease cleanup, and interrupted run recovery reliable
across supported platforms.

### [x] #1.3 Dashboard foundation

ID: m1.3
Depends on: m1.0

Introduce the HQ dashboard surface with stable prompt behavior, command output area, project/milestone panels,
error display, and backend launch preflight errors.

### [ ] #2.0 Shared milestone readiness

ID: m2.0
Depends on: m1.1

Add a reusable readiness calculation that classifies milestones as `ready`, `pending`, or `done`
based on completion and dependency state.

### [x] #2.1 Task origin metadata

ID: m2.1
Depends on: m1.0

Store `spec_path` and `milestone_id` on SQLite task rows so HQ can prevent duplicate queued or active tasks.

### [x] #2.2 Enqueue task tool

ID: m2.2
Depends on: m2.1

Add `/enqueue_task` for supervisor-driven queued task creation without entering the single active task state.

### [ ] #2.3 Run command planning

ID: m2.3
Depends on: m2.0, m2.1

Add `/run` and `/run --limit N` deterministic planning, including confirmation when fewer milestones are eligible
than the requested limit.

### [ ] #2.4 Batch task preparation

ID: m2.4
Depends on: m2.2, m2.3

Launch one interactive supervisor session with an exact HQ-selected milestone list and require approved task artifacts
for each selected milestone.

### [ ] #3.0 Multi-executor scheduling

ID: m3.0
Depends on: m2.4

Start queued tasks with executors up to the configured parallelism limit and track their runs through SQLite.

### [ ] #3.1 Worktree isolation

ID: m3.1
Depends on: m3.0

Run concurrent executors in isolated workspaces using `runs.workspace_path`.

### [ ] #3.2 Parallel review and integration

ID: m3.2
Depends on: m3.1

Review each task independently and add a supervisor-driven integration step for accepted parallel outputs.

### [ ] #4.0 SQLite state cutover

ID: m4.0
Depends on: m3.2

Remove `STATE.json` as the runtime source of truth after SQLite fully covers coordination, recovery,
active questions, active consultations, and active task identity.

## Acceptance Criteria

- The spec documents the current decisions, implemented pieces, and remaining work.
- Future `/run` implementation can use this spec as the source of intended behavior.
- Milestones are parseable by ferrus and include stable IDs and dependency metadata.
- The document preserves the Markdown vs SQLite boundary as an explicit architectural constraint.

## Risks and Open Questions

- The exact configured parallelism source still needs to be chosen.
- Worktree isolation may force earlier changes to executor launch paths than expected.
- The final integration policy needs concrete rules for conflicts, ownership, and partial failures.
- `STATE.json` cutover should wait until dashboard, ask-human, consultations, and recovery are all DB-backed.
- Agent environment propagation for deterministic task id and agent id must be verified per backend.
