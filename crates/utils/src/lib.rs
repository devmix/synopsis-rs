//! Cross-cutting helpers shared by several crates (change `utils-crate`,
//! design D1).
//!
//! Leaf of the dependency graph: any crate may depend on `utils`; it depends
//! on nothing internal. Modules group by concern; the first is
//! [`temporal`] — the project's single date/time seam over jiff (design D2).

pub mod temporal;
