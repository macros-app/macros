//! The Scheme primitives, one module per API category ([`super::Section`]).
//! Each module's `register` files its functions under its section(s);
//! `build_engine` (registry.rs) calls them in turn.

use super::*;
// `motions` shares the regex cache.
#[cfg(test)]
pub(super) use regexp::re_search_forward_in;
use regexp::regex;

pub(super) mod anvil;
pub(super) mod buffer_reads;
pub(super) mod buffers;
pub(super) mod crypto;
pub(super) mod dap;
pub(super) mod decorations;
pub(super) mod editing;
pub(super) mod faces;
pub(super) mod files;
pub(super) mod git;
pub(super) mod grid;
pub(super) mod help;
pub(super) mod hexbuf;
pub(super) mod hooks;
pub(super) mod http;
pub(super) mod keymaps;
pub(super) mod languages;
pub(super) mod lsp;
pub(super) mod markers;
pub(super) mod modal;
pub(super) mod motions;
pub(super) mod options;
pub(super) mod overlays;
pub(super) mod plotting;
pub(super) mod process;
pub(super) mod prompts;
pub(super) mod regexp;
pub(super) mod search;
pub(super) mod sockets;
pub(super) mod special_buffers;
pub(super) mod spell;
pub(super) mod sqlite;
pub(super) mod text;
pub(super) mod time;
pub(super) mod tree_sitter;
pub(super) mod ui;
