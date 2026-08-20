//! SQLite batch-size constants for the `db` crate (db-module design D9).
//!
//! SQLite bounds the number of bound parameters per statement at 32766. The
//! DAO layer batches `IN (...)` lists and multi-row `INSERT`s in chunks of
//! [`ID_BATCH_SIZE`] / [`LINK_BATCH_SIZE`] rows so that even a statement
//! carrying two parameter lists (500 × 2 = 1000) stays far below the limit.
//!
//! Single source of truth (human decision 2026-08-20, db-module task 1.18):
//! the `db` crate used to carry seven private copies of these values; they
//! now live here, and `db` depends on `config` for them.

/// Maximum ids per single `IN (...)` statement (design D9): SQLite bounds
/// bound parameters per statement at 32766; 500 stays far below it even when
/// one statement carries two `IN` lists (500 × 2 = 1000 parameters).
pub const ID_BATCH_SIZE: usize = 500;

/// Maximum rows per single multi-row `INSERT` statement (design D9): 500
/// rows × 2 columns = 1000 bound parameters, far below SQLite's 32766.
pub const LINK_BATCH_SIZE: usize = 500;
