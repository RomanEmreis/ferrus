# Ferrus Milestones

This document captures the intended direction for `ferrus`.
It is not a date-based roadmap. The goal is to keep the architectural direction clear
and to make the major dependencies between future milestones explicit.

## Guiding principles

- `ferrus` should remain a reliable orchestrator, not a fragile collection of scripts wrapped around LLMs.
- New capabilities must not weaken the core Supervisor-Executor loop for a single task.
- Major architectural changes should be introduced behind clear abstractions rather than hard-coding current implementation details.
- Local-first workflows matter. `ferrus` should work well without mandatory dependence on cloud-only services.
- The system should get more capable without forcing agents to repeatedly rediscover the same repository context from scratch.

## Milestone 1: Windows Support

Goal: make `ferrus` genuinely cross-platform so HQ, state management, agent spawning,
and checks work reliably on Linux, macOS, and Windows.

Why this belongs early:

- it materially expands where `ferrus` can be used;
- it forces POSIX-specific assumptions out of the core runtime;
- it prepares the foundation for more advanced orchestration features later.

Scope:

- audit and remove platform-specific assumptions around PTY handling, file locking, path handling, and process spawning;
- normalize shell and check execution for Windows environments;
- add Windows CI coverage;
- document the support policy and any known limitations.

Definition of done:

- the main `ferrus` commands work on Windows;
- the Supervisor-Executor loop runs end-to-end on Windows;
- CI validates support rather than relying on local assumptions.

## Milestone 2: Storage Layer and SQLite Backend

Goal: remove the direct coupling between runtime state and markdown/json files by introducing
a real storage layer, with SQLite as the primary backend for state, tasks, reviews, logs, and history.

Why this should come next:

- the current file-based model is excellent for bootstrapping and debugging, but weak for history, concurrency, and queryability;
- multi-agent orchestration will eventually require stronger consistency and richer state access patterns than ad-hoc files provide.

Scope:

- introduce a storage abstraction so core logic no longer talks directly to markdown/json files;
- add SQLite as the main runtime backend;
- provide a migration path from the current `.ferrus/` layout;
- preserve human-readable debugging surfaces where they remain useful;
- lay the groundwork for audit trails, event history, and richer HQ introspection.

Definition of done:

- the core workflow no longer depends directly on markdown/json files;
- SQLite is the source of truth for runtime state;
- the transition preserves the current user experience as much as practical.

## Milestone 3: Repository Graph and Indexed Context

Goal: build a repository graph during initialization and keep it available as a reusable structured context 
so agents can navigate the codebase faster and spend fewer tokens rediscovering the same information.

Why this matters:

- repeated repository exploration is expensive in both latency and tokens;
- orchestration gets stronger when agents can start from a shared structural understanding of the codebase;
- this becomes even more valuable once multiple agents are operating in parallel.

What the repository graph should eventually capture:

- file and module structure;
- symbols and their relationships where available;
- dependency edges between components;
- documentation and configuration entry points;
- a compact representation that can be queried incrementally rather than regenerated from scratch every run.

Open architectural question:

- this may live before SQLite as a file-backed index, or after SQLite as data stored in the main runtime backend;
- the more important point is to introduce a stable indexing abstraction rather than tying the feature too tightly to one storage format.

Definition of done:

- `ferrus init` or a follow-up indexing command can build repository context ahead of agent execution;
- agents can query this context instead of rescanning the entire repo by default;
- context retrieval is cheap enough to improve both token efficiency and practical task throughput.

## Milestone 4: Multi-Agent Flow

Goal: move from single-task execution to coordinated parallel work,
where multiple executors can operate independently and the supervisor manages decomposition and integration.

Target workflow:

- the supervisor breaks a large task into multiple work items;
- executors pick them up in parallel;
- each executor works in an isolated git worktree or equivalent isolated workspace;
- after each part passes review, the supervisor integrates the validated outputs into a coherent final result.

Why this depends on earlier milestones:

- multi-agent orchestration needs a stronger storage and coordination model;
- it benefits from indexed repository context, so each agent does not need to re-explore the whole codebase;
- merge and integration orchestration become much harder if the runtime still relies on loosely structured local files as the primary state container.

Scope:

- introduce a task graph or equivalent model for dependent and independent work items;
- support parallel executors;
- use git worktrees or equivalent isolation for concurrent code changes;
- add a supervisor-driven integration step after part-level reviews succeed;
- define conflict handling, retries, ownership, and HQ visibility for parallel execution.

Definition of done:

- one large task can be split and completed by multiple executors in parallel;
- each part runs through its own review loop;
- final integration is reproducible and understandable to the operator.

## Milestone 5: Ferrus Nano-Agent

Goal: add lightweight ferrus-native agents that `ferrus` can manage directly,
using local or remote LLMs without depending only on external coding agents.

Why this matters:

- it can reduce cost and latency for smaller or narrower subtasks;
- it opens the door to more specialized internal agent roles;
- it gives `ferrus` a path toward a more self-contained orchestration stack;
- it fits naturally with multi-agent workflows, where not every participant needs to be a heavyweight coding agent.

Scope:

- define a minimal runtime for ferrus-native agents;
- support both local and remote model providers;
- define a capability model for what nano-agents should and should not be trusted to do;
- integrate them safely into the existing orchestration loop;
- measure quality, cost, and reliability relative to external agents.

Definition of done:

- `ferrus` can launch its own mini-agents as first-class orchestration participants;
- there is at least one practical workflow where nano-agents improve cost, speed, or quality.

## Additional milestones worth considering

These are not necessarily standalone roadmap phases, but they strongly support the main milestones above.

### Event log and observability

As `ferrus` moves toward SQLite and multi-agent execution, it will likely need a proper event log:
state transitions, claims, heartbeats, retries, review outcomes, and integration steps.
That will make debugging, replay, and HQ visibility much more robust.

### Pluggable execution and runtime interfaces

The earlier orchestration logic is separated from any one executor implementation,
the easier it becomes to support nano-agents, multiple model backends, and future execution strategies.

### Task decomposition and merge policy

Parallelism alone is not enough. Multi-agent flow will depend on good decomposition quality:
how the supervisor splits work, how contracts between parts are defined, and how final integration happens without chaos.
This may deserve its own design track rather than living only inside the multi-agent milestone.

## Proposed order

1. Windows support
2. Storage layer + SQLite
3. Repository graph and indexed context
4. Event log / observability
5. Multi-agent flow
6. Ferrus nano-agent

This is not the only reasonable order, but it reduces the risk that multi-agent and nano-agent work
will be built on top of a runtime foundation that is still too fragile.

## Non-goals for now

- turning this roadmap into a date-driven quarterly plan;
- committing to delivery dates before the core architecture stabilizes;
- adding major product surface area before strengthening the orchestration core.
