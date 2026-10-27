//! The Steel scripting layer, split by concern:
//!   * [`commands`] — the [`EditorCommand`] queue vocabulary and its payloads.
//!   * [`state`] — shared editor state published to the engine thread
//!     (snapshots, markers, options).
//!   * [`gix`] — pure git backends (log/diff/status/…) with no Steel coupling.
//!   * [`engine`] — the [`ScriptEngine`] handle and its dedicated thread.
//!   * [`registry`] — `build_engine`, the [`Registrar`] and its helpers.
//!   * [`prims`] — the registered Scheme primitives, one module per category.
//!   * [`search`] — ripgrep-library file walking for the search primitives.
//!   * [`convert`] — SteelVal parsing/formatting helpers.
//!   * [`session`] — session-persistence primitives (savehist / desktop).
//!   * [`sockets`] — socket / SQLite / DNS backends and their handle tables.
//!   * [`api`] — the package-API reference (categories, types, docs).

mod api;
mod commands;
mod convert;
mod engine;
mod gix;
mod prims;
mod registry;
mod search;
mod session;
mod sockets;
mod state;

pub(crate) use api::{ApiEntry, Section, render as render_api};
pub use commands::*;
pub(crate) use convert::*;
pub use engine::*;
pub(crate) use gix::*;
#[cfg(test)]
use prims::re_search_forward_in;
pub(crate) use registry::*;
pub(crate) use search::*;
pub(crate) use session::*;
pub(crate) use sockets::*;
pub use state::*;

#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod shell_grep_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod wdired_tests;
