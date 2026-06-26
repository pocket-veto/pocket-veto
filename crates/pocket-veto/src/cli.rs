//! CLI definition: the clap [`Cli`] / [`Command`] structs, the per-subcommand
//! arg structs ([`ServeArgs`] / [`HookArgs`] / [`InitArgs`]), the shared
//! [`Ctx`], and the [`Subcommand`] dispatch trait.
//!
//! `main.rs` parses [`Cli`], initializes tracing, and calls
//! [`Command::run`], which delegates to the matched arg struct's
//! [`Subcommand::run`] impl. The `run` impls live next to the subcommand
//! logic (`init`, `hook`, `serve`) so this module is purely the dispatch
//! shell plus the clap schema.

use std::process::ExitCode;

use clap::{Args, Parser};

/// Top-level CLI parsed from `argv`.
#[derive(Parser)]
#[command(
    name = "pocket-veto",
    version,
    about = "Local Bluetooth-mediated approval gate for AI coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The three subcommands. Each variant wraps a per-subcommand arg struct so
/// the [`Subcommand`] trait can be implemented on the arg struct directly.
///
/// Uses the fully-qualified `clap::Subcommand` derive so the local
/// [`Subcommand`] dispatch trait can share the bare name without clashing
/// (Rust 2024 allows path-qualified derive macros).
#[derive(clap::Subcommand)]
pub enum Command {
    /// Start the dashboard server and bridge.
    Serve(ServeArgs),
    /// Run as an agent hook (`PreToolUse` / `PostToolUse`) for approval gating.
    Hook(HookArgs),
    /// Initialize configuration and onboarding.
    Init(InitArgs),
}

impl Command {
    /// Default `EnvFilter` level for the matched subcommand (used only when
    /// `RUST_LOG` is unset). The server is chattier (`info`) so the operator
    /// sees request/lifecycle logs; the hook and init stay quiet (`warn`) so
    /// the hook's stdout stays clean for the agent host and onboarding output
    /// stays readable.
    #[must_use]
    pub fn default_log_filter(&self) -> &'static str {
        match self {
            Self::Serve(_) => "info",
            Self::Hook(_) | Self::Init(_) => "warn",
        }
    }
}

/// `pocket-veto serve` (no extra flags; config is loaded from
/// `~/.pocket-veto/config.toml`).
#[derive(Args)]
pub struct ServeArgs;

/// `pocket-veto hook` (no extra flags; the event arrives on stdin).
#[derive(Args)]
pub struct HookArgs;

/// `pocket-veto init` — interactive onboarding. Every flag has a matching
/// interactive prompt, so a user can run `pocket-veto init` bare or pre-fill
/// any value with a flag.
#[derive(Args)]
pub struct InitArgs {
    /// Path to the pocket-veto binary to embed in hook commands (default:
    /// `pocket-veto` on PATH).
    #[arg(long, default_value = "pocket-veto")]
    pub bin_path: String,
    /// Reuse the existing bearer token instead of generating a new one.
    #[arg(long)]
    pub keep_token: bool,
    /// Enable devcontainer support (binds 0.0.0.0 instead of 127.0.0.1).
    #[arg(long)]
    pub devcontainer: bool,
    /// Skip the interactive BT pairing prompts (use for headless setup).
    #[arg(long)]
    pub skip_bt: bool,
    /// Linux RFCOMM channel (skip the prompt).
    #[arg(long)]
    pub bt_channel: Option<u8>,
    /// Windows COM port (skip the prompt).
    #[arg(long)]
    pub bt_com_port: Option<String>,
    /// Linux adapter address (skip the prompt).
    #[arg(long)]
    pub bt_adapter_addr: Option<String>,
}

/// Shared context handed to every [`Subcommand::run`] impl. Currently empty
/// (each subcommand loads its own config and owns its I/O); kept as an
/// explicit type so future shared state (a loaded config, a runtime handle)
/// can be added without changing the trait signature.
#[derive(Debug, Clone, Copy)]
pub struct Ctx;

/// Dispatch trait: each subcommand's arg struct implements `run` to execute
/// the subcommand and report a process exit code. Uses native
/// async-fn-in-trait (AFIT, stable on 1.96) — no `async_trait` crate.
///
/// `run` returns [`anyhow::Result`] (the binary boundary) so
/// infrastructure errors propagate; a successful deny/timeout still returns
/// `Ok(ExitCode)` carrying the non-zero exit code the hook contract requires
/// (exit 2 = deny / fail-closed, exit 0 = allow / ask / non-blocking).
///
/// `main` awaits `run` directly on the main thread (`block_on`), so the
/// returned future does not need to be `Send`; the `async_fn_in_trait` lint
/// is therefore silenced with a reason rather than desugaring to
/// `impl Future + Send`.
pub trait Subcommand {
    /// Execute the subcommand.
    ///
    /// # Errors
    ///
    /// Returns `Err` only for infrastructure failures (config load, bind,
    /// fatal server error). A hook deny or timeout is a successful `Ok`
    /// carrying the appropriate [`ExitCode`].
    #[allow(
        async_fn_in_trait,
        reason = "main awaits run directly via block_on; the future does not need to be Send"
    )]
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<ExitCode>;
}

impl Subcommand for Command {
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<ExitCode> {
        match self {
            Self::Serve(a) => a.run(ctx).await,
            Self::Hook(a) => a.run(ctx).await,
            Self::Init(a) => a.run(ctx).await,
        }
    }
}
