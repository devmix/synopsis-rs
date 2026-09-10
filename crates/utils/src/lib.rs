//! Cross-cutting helpers shared by several crates (change `utils-crate`,
//! design D1).
//!
//! Leaf of the dependency graph: any crate may depend on `utils`; it depends
//! on nothing internal. Modules group by concern: [`temporal`] — the
//! project's single date/time seam over jiff (design D2), and [`text`] —
//! script-based word stemming shared by `ingestion` and `graph` (change
//! `multilingual-entity-resolution`, design D1).

pub mod temporal;
pub mod text;
