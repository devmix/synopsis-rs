//! Per-tool handlers (design D2/D4): each module is a thin
//! parse-args → crate-API → oracle-shaped-JSON function, registered in
//! [`crate::server::Server::dispatch`].

pub mod catalog;
pub mod documents;
pub mod entities_catalog;
pub mod facts;
pub mod search;
