//! The formula engine for grid buffers: Excel's formula language, evaluated
//! with incremental recalculation. See `docs/plans/formula-engine.md`.
//!
//! Written from scratch on `std` alone — no parsing, regex, date, or decimal
//! crates — so its behaviour is exactly what's here.

pub mod ast;
pub mod criteria;
pub mod date;
pub mod decimal;
pub mod engine;
pub mod eval;
pub mod func;
pub mod implicit;
pub mod parse;
pub mod print;
pub mod refs;
pub mod textfmt;
pub mod value;
