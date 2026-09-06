# llm-client Specification

## Purpose

OpenAI-compatible LLM client: a single call point for chat completions with retries, a timeout, and structured output, for the entity linker and future NER.

## Requirements

### Requirement: Chat completions call

The `llm` crate provides a synchronous client of the OpenAI-compatible API `POST {api_base_url}/chat/completions`: the request body contains model, messages (system + user), temperature, seed, max_tokens; authentication is a Bearer token from `api_key` (an empty key — the header is not sent); the response is parsed into the text of the first choice. The client is configured from `LlmConfig` (base URL, model, temperature, seed, max_tokens, timeout, retries).

#### Scenario: Successful call
- **WHEN** a well-formed server returns choices[0].message.content
- **THEN** the client returns the text; the request contains model/messages/temperature/seed/max_tokens and the Bearer header

#### Scenario: Empty api_key
- **WHEN** the api_key in the config is empty
- **THEN** the Authorization header is not sent; the call is made

### Requirement: Structured output

The `response_format` mode from the config controls the request_format field: `json_object` → `{"type":"json_object"}`; `json_schema` → `{"type":"json_schema","json_schema":{"name":N,"schema":S}}`, where the schema is passed by the calling code together with its name (default "llm_output").

#### Scenario: json_object
- **WHEN** response_format=json_object and no schema is given
- **THEN** the request contains only {"type":"json_object"}

#### Scenario: json_schema
- **WHEN** response_format=json_schema and a name and schema are passed
- **THEN** the request contains the nested json_schema object with the name and schema

### Requirement: Retries and timeouts

Transient failures are retried up to `max_retries` times with exponential backoff and jitter: HTTP 429 and 5xx are retryable; empty content in a valid response is an explicit non-retryable error; network errors/timeouts are retryable. The timeout of a single request comes from the config. Exhausting the attempts is an explicit error carrying the last failure's reason.

#### Scenario: Retry on 429/5xx
- **WHEN** the server answers 429 or 5xx
- **THEN** a retry is made (up to the limit) with a growing delay; a success after a retry returns the result

#### Scenario: Empty content without retry
- **WHEN** a valid response contains empty content
- **THEN** an explicit error is returned immediately, without retries

#### Scenario: Attempts exhausted
- **WHEN** all attempts end in a transient failure
- **THEN** an error is returned indicating the last reason

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
