# pipeline Specification

## ADDED Requirements

### Requirement: Worker job logging

The document worker logs every job's outcome through structured logging:
info on success (path + op), warn on failure with the attempt count and
backoff, error when the retry cap is reached; failures to record success,
unknown ops, and orphan-cleanup failures are logged at warn/error. After
each cycle that processed one or more jobs, the worker logs a queue-state
summary (counts by status). Runner per-document warnings use the same
structured logging.

#### Scenario: Successful job
- **WHEN** a job's document is processed successfully
- **THEN** an info event carries the document path and the op

#### Scenario: Failed job below the cap
- **WHEN** a job fails and has retry budget left
- **THEN** a warn event carries the path, the attempt number, and the backoff

#### Scenario: Cycle summary
- **WHEN** a worker cycle processes one or more jobs
- **THEN** an info event carries the processed count and the queue counts by status
