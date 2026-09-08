# cli-surface Specification

## MODIFIED Requirements

### Requirement: serve subcommand

The only long-running mode: startup reconcile (enqueue diff into `queue_tasks`) + MCP over HTTP (Streamable HTTP, design D8) + file watching (enqueue diff). Flags: `--no-initial-sync` (skip startup reconcile at boot), `--port N` (default 8080, overrides server.port from config), `--auto-rebuild-vectors`. On a vector-engine dimension mismatch, serve clears the dataset DB (`clear_dataset`) and the startup reconcile re-enqueues all files — the worker re-embeds them (clear-then-queue, no direct ingestion).

Shutdown is signal-driven and cooperative. The first SIGINT or SIGTERM starts a graceful stop: the in-flight queue task (if any) runs to completion and is recorded, the worker stops claiming new tasks, the HTTP server drains in-flight requests, the vector RAM layer is saved, and the process exits with code 0. This applies to every worker cycle, including the startup drain (the server is stoppable while the initial sync is still processing the queue). As part of the graceful stop the server terminates its own long-lived legacy SSE sessions (the streams end, connected MCP clients see a clean EOF) — an open SSE stream is an in-flight request that would otherwise keep the HTTP drain open until the 10 s bound, so the drain completes promptly even with connected clients. A second SIGINT or SIGTERM at any time forces an immediate exit with code 130 (128 + SIGINT), abandoning the in-flight work — the restart recovery (stuck `processing` rows reset to `pending`) and the startup vector self-heal repair any residual state, so the forced exit is safe.

#### Scenario: Port

- **WHEN** serve --port 9123
- **THEN** the HTTP server listens on 9123, GET /health returns 200

#### Scenario: Graceful stop mid-ingestion

- **WHEN** SIGINT is received while the worker is processing a queue task and more tasks are pending
- **THEN** the in-flight task completes and is recorded, no further tasks are claimed, the server stops, the process exits with code 0, and the unclaimed tasks remain `pending` for the next startup

#### Scenario: Stop during the startup drain

- **WHEN** SIGINT is received during the startup worker drain (before the owner loop serves traffic)
- **THEN** the drain stops after the in-flight task, the serve flow proceeds to the bounded shutdown, and the process exits with code 0

#### Scenario: Graceful stop with an active SSE session

- **WHEN** SIGINT is received while a legacy SSE MCP session is connected (no queue task in flight)
- **THEN** the server terminates the SSE session (the stream ends, the client sees a clean EOF), the HTTP drain completes within the bounded shutdown (well under the 10 s bound), and the process exits with code 0

#### Scenario: Second signal forces exit

- **WHEN** a second SIGINT or SIGTERM is received after the first one (while the graceful stop is still finishing the in-flight task)
- **THEN** the process exits immediately with code 130
