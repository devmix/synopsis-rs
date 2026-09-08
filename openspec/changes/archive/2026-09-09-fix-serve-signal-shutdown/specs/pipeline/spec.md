# pipeline Specification

## ADDED Requirements

### Requirement: Cancellable worker cycle

The document-queue worker cycle SHALL check a shared shutdown flag before
claiming each task (including the first claim of the cycle). A worker
constructed without a flag checks nothing (current behavior). When the flag
is set, the cycle stops claiming new tasks and completes its tail as usual
(post-cycle GC, per-cycle vector persistence when work happened, cycle
summary). A task already in flight when the flag is set runs to completion
and is recorded exactly as usual (`done`, or backoff/`error` on failure),
bounding the shutdown delay to one task. Tasks that were never claimed stay
`pending` in `queue_tasks` and are picked up on a later startup (the restart
recovery and the startup reconcile already cover pending rows).

#### Scenario: Flag set before the cycle

- **WHEN** the shutdown flag is set before a worker cycle starts and pending tasks exist in the queue
- **THEN** the cycle claims no tasks, all tasks remain `pending`, and the cycle completes normally

#### Scenario: Flag set mid-cycle

- **WHEN** the shutdown flag is set after a worker cycle has claimed its first task and more tasks are pending
- **THEN** the in-flight task runs to completion and is marked `done`, the cycle claims no further tasks, and the remaining tasks stay `pending`

#### Scenario: In-flight failure during shutdown

- **WHEN** the shutdown flag is set while a task is in flight and that task fails
- **THEN** the failure is recorded with the usual backoff/error semantics (the flag changes claim behavior, not failure recording)

#### Scenario: Persistence on a cancelled work cycle

- **WHEN** a worker cycle processed one or more tasks before the flag stopped the claims
- **THEN** the vector RAM layer is persisted after the cycle (the existing per-cycle save, vector-loss-self-heal D1)

#### Scenario: Worker without a flag

- **WHEN** a worker is constructed without a shutdown flag
- **THEN** the cycle behaves exactly as before this requirement (claims up to the per-cycle cap, no flag checks)
