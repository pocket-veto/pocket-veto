// justification: this is the CLI binary crate; stdout is its primary user-facing
// output channel (subcommand results, `init` diagnostics, hook stdout). Banning
// `println!` here would defeat the crate's purpose.
#![allow(clippy::print_stdout)]
//! `pocket-veto` library target.
//!
//! The binary's logic lives in modules here so that integration tests (and
//! future embedding use cases) can import them as `pocket_veto::...`. The
//! `main.rs` binary is a thin wrapper: it parses the CLI ([`cli::Cli`]),
//! initializes tracing once via [`init_tracing`], and dispatches to the
//! matched subcommand's [`cli::Subcommand::run`] impl.
//!
//! Subcommand arg structs ([`cli::ServeArgs`] / [`cli::HookArgs`] /
//! [`cli::InitArgs`]) and the [`cli::Subcommand`] dispatch trait live in
//! [`cli`]; each `run` impl lives next to the subcommand logic it delegates
//! to (`init`, `hook`, `serve`) so the CLI layer stays a thin dispatch shell.

use std::sync::OnceLock;

pub mod cli;
pub mod hook;
pub mod init;
pub mod serve;

// Re-export the CLI types so `main.rs` (and tests) can reach them as
// `pocket_veto::{Cli, Command, Ctx, Subcommand}` without drilling into the
// `cli` module.
pub use cli::{Cli, Command, Ctx, Subcommand};

// Re-export the core timestamp newtype so every subcommand shares one typed
// timestamp helper (don't reimplement std; no cross-crate duplication of the
// `now_ms` body).
pub use pocket_veto_core::TimestampMs;

/// Current unix-epoch milliseconds. Thin shim over
/// [`TimestampMs::now`] so the binary's subcommands share a single timestamp
/// helper instead of each redefining their own `now_ms`.
#[must_use]
pub fn now_ms() -> i64 {
    TimestampMs::now().0
}

/// Initialize the `tracing_subscriber` once. The first call wins; subsequent
/// calls are no-ops (a [`OnceLock`] guards the registration), so re-entry from
/// tests or repeated subcommand dispatch does not panic on a second
/// `try_init`.
///
/// `default_filter` is the [`tracing_subscriber::EnvFilter`] used when
/// `RUST_LOG` is unset. The server passes `"info"` (so the operator sees
/// request/lifecycle logs); the hook and init pass `"warn"` so the hook's
/// stdout stays clean for the agent host and onboarding output stays
/// readable. `tracing_subscriber::fmt` writes to stderr, so the hook's
/// JSON-on-stdout contract is unaffected either way.
///
/// This is the single source of truth for subscriber setup: `init`, `hook`,
/// and `serve` all call through here rather than each keeping their own copy.
pub fn init_tracing(default_filter: &str) {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
        // `try_init` returns `Err` if a global subscriber is already set
        // (e.g. when the hook is invoked inside a test harness); ignore.
        let _init = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    });
}
