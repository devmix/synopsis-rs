# llm-client Specification

## ADDED Requirements

### Requirement: Response truncation detection

A 2xx response whose first choice has non-empty content and a
`finish_reason` of `length` is a truncated response: the client returns an
explicit non-retryable truncation error carrying the configured
`max_tokens`, without retrying. Other `finish_reason` values are unaffected.

#### Scenario: Truncated response
- **WHEN** the server returns 200 with non-empty content and `finish_reason` `length`
- **THEN** the client returns a truncation error naming the configured `max_tokens`, without retries

#### Scenario: Normal completion
- **WHEN** the server returns 200 with non-empty content and `finish_reason` `stop` (or absent)
- **THEN** the content is returned as before
