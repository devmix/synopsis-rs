//! Handler error type (design D7): wraps collaborator crate errors and maps
//! them to MCP *tool* error results — the caller-visible failure mode with a
//! human-readable message. Panics never cross the handler boundary.

use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};

/// MCP handler error (design D7).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// Storage-layer failure (db crate).
    #[error(transparent)]
    Db(#[from] db::DbError),
    /// Search-pipeline failure (search crate).
    #[error(transparent)]
    Search(#[from] search::SearchError),
    /// Knowledge-graph failure (graph crate).
    #[error(transparent)]
    Graph(#[from] graph::GraphError),
    /// A registered tool whose handler is not implemented yet (tasks 5.3–5.9).
    #[error("tool '{0}' is not implemented yet")]
    NotYetImplemented(String),
    /// Argument parse/validation failure for a registered tool.
    #[error("invalid arguments for tool '{tool}': {reason}")]
    InvalidArguments {
        /// The tool the arguments belong to.
        tool: &'static str,
        /// What was wrong with the arguments.
        reason: String,
    },
}

impl McpError {
    /// Render as an MCP *tool* error result (`is_error = true` content block)
    /// so the caller reads the message (design D7). The Go oracle handlers
    /// do the same: `CallToolResult{IsError: true}` with the message text.
    /// (A protocol-level `Err(ErrorData)` would hide the message from the
    /// client — reserved for unroutable requests like unknown tools.)
    pub fn into_tool_result(self) -> CallToolResponse {
        let message = self.to_string();
        CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(message)]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use rmcp::model::CallToolResponse as Resp;

    #[test]
    fn into_tool_result_is_error_result_with_message() {
        let result = McpError::NotYetImplemented("search".to_owned()).into_tool_result();
        let Resp::Complete(call) = result else {
            panic!("expected a complete tool result");
        };
        assert_eq!(call.is_error, Some(true));
        let text = call
            .content
            .first()
            .and_then(rmcp::model::ContentBlock::as_text)
            .unwrap();
        assert!(text.text.contains("search"), "got: {}", text.text);
        assert!(text.text.contains("not implemented"), "got: {}", text.text);
    }

    #[test]
    fn invalid_arguments_message_names_tool_and_reason() {
        let err = McpError::InvalidArguments {
            tool: "get_chunk_by_id",
            reason: "chunk_id is required".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "invalid arguments for tool 'get_chunk_by_id': chunk_id is required"
        );
    }

    #[test]
    fn collaborator_errors_wrap_transparently() {
        let db_err = McpError::from(db::DbError::NestedTransaction);
        assert!(db_err.to_string().contains("nested transaction"));
        let graph_err = McpError::Graph(graph::GraphError::EmptyQuery { what: "name" });
        assert_eq!(graph_err.to_string(), "empty query: name must not be empty");
    }
}
