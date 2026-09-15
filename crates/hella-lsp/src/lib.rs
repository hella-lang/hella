//! hella-lsp — the Hella Language Server (library crate).
//!
//! Speaks LSP over stdio and reuses the `compiler` crate for the actual
//! lex/parse/sema pipeline so diagnostics and symbol data stay consistent
//! with the `hella` command-line toolchain.
//!
//! Consumed two ways:
//! - embedded in the `hella` CLI via `hella lsp` (alias `hella ls`)
//! - embeddable in other tooling by calling [`server::run`] directly

pub mod analysis;
pub mod diagnostics;
pub mod document;
pub mod formatting;
pub mod progress;
pub mod server;
pub mod signatures;