//! `pocket-veto init` — interactive one-shot onboarding.
//!
//! Walks the user through the seven setup steps:
//!
//! 1. Greet & detect an existing config (offer to keep the existing token).
//! 2. Bluetooth pairing guidance (platform-specific, stubbed on Linux/macOS,
//!    COM-port enumeration on Windows).
//! 3. Channel / COM port detection (folded into step 2; the discovered value
//!    is stored on the [`InitOpts`] and applied to the [`Config`]).
//! 4. Bearer-token generation (new by default, reused when `--keep-token`).
//! 5. Hook config installation — Cursor `.cursor/hooks.json` (overwrite after
//!    backup) and Claude Code `.claude/settings.json` (merge `hooks` into any
//!    existing file).
//! 6. Service registration — write the platform's service file
//!    (`systemd`/`launchd`/Scheduled-Task guidance) and *print* the command the
//!    user should run. Init never invokes `systemctl`/`launchctl`/`reg` itself
//!    (no sudo/DBUS needed, keeps init testable).
//! 7. Devcontainer support prompt — binds `0.0.0.0` and prints the
//!    `host.docker.internal` URL plus the sample `devcontainer.json` snippet.
//!
//! Then writes `~/.pocket-veto/config.toml` via [`Config::save`].
//!
//! # Testability
//!
//! [`Subcommand::run`] (the [`crate::cli::InitArgs`] impl) is a thin
//! interactive wrapper. It resolves the user's answers into an [`InitOpts`]
//! struct and then delegates every file-writing step to a **pure** function
//! that takes explicit paths and does no stdin/stdout I/O:
//!
//! - [`build_config`] — fold opts + detected BT params into a [`Config`].
//! - [`write_cursor_hooks`] — write `.cursor/hooks.json` (overwrite + backup).
//! - [`write_claude_hooks`] — merge `hooks` into `.claude/settings.json`.
//! - `write_systemd_unit` — Linux: write the user systemd unit.
//! - `write_launchd_plist` — macOS: write the `LaunchAgent` plist.
//!
//! (`write_systemd_unit` / `write_launchd_plist` are `#[cfg]`-gated to their
//! platform, so they are referenced here as plain backticks rather than
//! intra-doc links — a bracketed link would be broken on the other platform.)
//!
//! Integration tests in `tests/init_subcommand.rs` drive the pure functions
//! directly against `tempfile::tempdir()` roots, so none of the interactive
//! prompts need to be wired up to a fake stdin.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use pocket_veto_core::config::{BtBackend, Config};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::cli::{Ctx, InitArgs, Subcommand};

/// Resolved options for `init`, derived from CLI flags plus interactive
/// prompts. [`Subcommand::run`] (the [`InitArgs`] impl) builds this and then
/// hands it to the pure helpers, so tests can construct one directly and skip
/// the prompts.
#[derive(Debug, Clone)]
pub struct InitOpts {
    /// Path used in hook commands (default `pocket-veto` on PATH; may be an
    /// absolute path when the binary is not on PATH).
    pub bin_path: String,
    /// Reuse the existing config's bearer token instead of generating a new
    /// one. Ignored when `existing_config` is `None`.
    pub keep_token: bool,
    /// Bind `0.0.0.0:38475` for devcontainer access and print the
    /// `host.docker.internal` snippet.
    pub devcontainer: bool,
    /// Windows COM port of the paired SPP device (e.g. `COM3`).
    pub bt_com_port: Option<String>,
    /// Linux Bluetooth adapter address (e.g. `AA:BB:CC:DD:EE:FF`).
    pub bt_adapter_addr: Option<String>,
    /// Linux RFCOMM channel number.
    pub bt_channel: Option<u8>,
    /// The config saved by a prior `init` run, if `init` is being re-run.
    /// Used by [`build_config`] when `keep_token` is true.
    pub existing_config: Option<Config>,
}

impl Subcommand for InitArgs {
    #[allow(
        clippy::unused_async,
        reason = "InitArgs::run is async by Subcommand-trait contract (shared with Hook/Serve, which do await); init's I/O is fully synchronous, so there is no .await in this body"
    )]
    async fn run(&self, _ctx: &Ctx) -> anyhow::Result<std::process::ExitCode> {
        // Resolve any BT params that were not provided as flags by prompting
        // interactively FIRST (BT prompts run before the greeting).
        // `--skip-bt` skips all BT prompts (headless / devcontainer-only
        // setups).
        let (bt_channel, bt_com_port, bt_adapter_addr) = if self.skip_bt {
            (
                self.bt_channel,
                self.bt_com_port.clone(),
                self.bt_adapter_addr.clone(),
            )
        } else {
            resolve_bt_params_interactively(
                self.bt_channel,
                self.bt_com_port.clone(),
                self.bt_adapter_addr.clone(),
            )
        };

        // If the user is re-running init and wants to keep the existing token,
        // load the existing config so build_config can reuse it. The
        // overwrite-confirm step below may also re-load it when the user opts
        // into keep-token mid-flow.
        let existing_config = if self.keep_token {
            Config::config_path().and_then(|p| Config::load(&p)).ok()
        } else {
            None
        };

        let mut opts = InitOpts {
            bin_path: self.bin_path.clone(),
            keep_token: self.keep_token,
            devcontainer: self.devcontainer,
            bt_com_port,
            bt_adapter_addr,
            bt_channel,
            existing_config,
        };

        // `run_init_flow` is synchronous (init does only blocking fs + stdin
        // I/O); the surrounding `async fn` exists only to satisfy the
        // `Subcommand` trait's async signature shared with Hook/Serve.
        run_init_flow(&mut opts)?;
        Ok(std::process::ExitCode::SUCCESS)
    }
}

/// The interactive + file-writing flow, factored out of [`Subcommand::run`]
/// so it takes a single mutable [`InitOpts`] and can be reasoned about in
/// isolation. Performs the overwrite-confirm / keep-token / devcontainer
/// prompts, prints BT guidance, writes hook configs, prints service
/// registration, builds and saves the config, and prints a summary.
///
/// Synchronous by design: every step is blocking fs or stdin I/O, so there is
/// no `async`/`await` here (the `Subcommand::run` wrapper is async only to
/// satisfy the trait signature shared with Hook/Serve).
///
/// # Errors
///
/// Returns `Err` if any file write or config save fails. Interactive prompt
/// I/O failures are also propagated (a closed stdin is not recoverable).
fn run_init_flow(opts: &mut InitOpts) -> anyhow::Result<()> {
    println!(
        "pocket-veto init — local Bluetooth-mediated approval gate setup\n\
         ================================================================\n"
    );

    // Step 1 — detect existing config. Confirm overwrite (the user can abort
    // by answering "n"). When `--keep-token` was passed, `existing_config`
    // is already populated; otherwise this step offers to keep the token and
    // loads it on acceptance.
    let config_path = Config::config_path()?;
    if config_path.exists() {
        println!(
            "An existing config was found at {}\n\
             Re-running init will OVERWRITE it.\n",
            config_path.display()
        );
        if !prompt_yes_no("Proceed with overwrite?", true)? {
            println!("Aborted; no changes made.");
            return Ok(());
        }
        if !opts.keep_token {
            opts.keep_token = prompt_yes_no("Keep the existing bearer token?", true)?;
        }
        if opts.keep_token && opts.existing_config.is_none() {
            opts.existing_config = Config::config_path().and_then(|p| Config::load(&p)).ok();
        }
    }

    // Step 7 — Devcontainer support. If the flag was not passed, ask.
    if !opts.devcontainer {
        opts.devcontainer = prompt_yes_no("Enable devcontainer support (bind 0.0.0.0)?", false)?;
    }

    // Step 2/3 — Bluetooth pairing guidance. The actual discovered values
    // arrive on `opts` (from CLI flags or interactive prompts); this step
    // just prints platform-specific guidance so the user knows what to do.
    print_bt_guidance(opts);

    // Step 5 — Hook config installation. Detect installed agent hosts in the
    // cwd and the user's home dir, then write the configs.
    let install_targets = resolve_hook_install_dirs();
    for dir in &install_targets {
        if dir.join(".cursor").exists() {
            write_cursor_hooks(dir, &opts.bin_path)
                .with_context(|| format!("write .cursor/hooks.json in {}", dir.display()))?;
            println!("  wrote .cursor/hooks.json in {}", dir.display());
        }
        if dir.join(".claude").exists() {
            write_claude_hooks(dir, &opts.bin_path)
                .with_context(|| format!("write .claude/settings.json in {}", dir.display()))?;
            println!("  wrote .claude/settings.json in {}", dir.display());
        }
    }

    // Step 6 — Service registration. Write the platform-specific service file
    // and print the command the user should run (never invoke it ourselves).
    print_service_registration(&opts.bin_path)?;

    // Step 7 (snippet) — print the devcontainer hint after the file writes.
    if opts.devcontainer {
        print_devcontainer_snippet();
    }

    // Step 8 — Build & write the config.
    let cfg = build_config(opts);
    cfg.save(&config_path)
        .context("save ~/.pocket-veto/config.toml")?;

    println!("\nConfig written to {}", config_path.display());
    print_summary(&cfg, opts);
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure helpers (no stdin/stdout, no env reads) — directly unit-testable.
// ---------------------------------------------------------------------------

/// Fold [`InitOpts`] into a [`Config`] ready to be saved.
///
/// - Starts from [`Config::default`].
/// - Applies the devcontainer bind address when `opts.devcontainer` is set.
/// - Applies the BT params (`bt_com_port` / `bt_adapter_addr` / `bt_channel`).
/// - Selects the BT backend based on the platform + which BT field is set.
/// - Reuses the existing token when `opts.keep_token` and an existing config
///   is supplied; otherwise keeps the freshly-generated default token.
#[must_use]
pub fn build_config(opts: &InitOpts) -> Config {
    let mut cfg = Config::default();

    if opts.devcontainer {
        cfg.devcontainer = true;
        cfg.bind_addr = "0.0.0.0:38475".to_string();
    }

    cfg.bt_com_port.clone_from(&opts.bt_com_port);
    cfg.bt_adapter_addr.clone_from(&opts.bt_adapter_addr);
    cfg.bt_channel = opts.bt_channel;

    // Pick the backend based on which BT field is set, defaulting to the
    // platform-native choice. Windows users configure a COM port; Linux users
    // configure an adapter address and channel.
    cfg.bt_backend = pick_backend(&cfg);

    if opts.keep_token
        && let Some(existing) = &opts.existing_config
    {
        cfg.token.clone_from(&existing.token);
    }

    cfg
}

/// Choose the BT backend based on which BT field is populated. Defaults to
/// `Bluer` on Linux and `Serialport` on Windows; if a COM port is set, forces
/// `Serialport`; if an adapter addr or channel is set, forces `Bluer`.
fn pick_backend(cfg: &Config) -> BtBackend {
    if cfg.bt_com_port.is_some() {
        return BtBackend::Serialport;
    }
    if cfg.bt_adapter_addr.is_some() || cfg.bt_channel.is_some() {
        return BtBackend::Bluer;
    }
    // No BT info: keep the platform-native default.
    #[cfg(target_os = "windows")]
    {
        BtBackend::Serialport
    }
    #[cfg(not(target_os = "windows"))]
    {
        BtBackend::Bluer
    }
}

// ---------------------------------------------------------------------------
// Cursor `.cursor/hooks.json` — typed serde structs (no stringly-typed
// `Map<String, Value>` mutation for a fixed schema).
// ---------------------------------------------------------------------------

/// The top-level `.cursor/hooks.json` file. Serialized with camelCase hook
/// keys to match Cursor's schema exactly.
#[derive(Serialize)]
struct CursorHooksFile {
    version: u32,
    hooks: CursorHooks,
}

/// The `hooks` object inside `.cursor/hooks.json`. Field names are
/// `#[serde(rename_all = "camelCase")]` to match Cursor's event keys
/// (`beforeShellExecution`, `preToolUse`, ...).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorHooks {
    before_shell_execution: Vec<CursorHookEntry>,
    pre_tool_use: Vec<CursorHookEntry>,
    post_tool_use: Vec<CursorHookEntry>,
    after_agent_thought: Vec<CursorHookEntry>,
    stop: Vec<CursorHookEntry>,
    session_start: Vec<CursorHookEntry>,
}

/// One entry under a Cursor hook event. `matcher` and `fail_closed` are
/// omitted when `None` so the non-blocking hooks serialize without them.
/// `Clone` so the shared non-blocking entry can be reused across the four
/// non-blocking events.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CursorHookEntry {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    matcher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fail_closed: Option<bool>,
}

/// Build the [`CursorHooksFile`] for the given command + pre-tool matcher.
/// `matcher` is applied only to `preToolUse`; the blocking
/// `beforeShellExecution` entry carries just `command` + `failClosed`, and the
/// non-blocking entries carry just `command`.
fn cursor_hooks_file(command: &str, matcher: &str) -> CursorHooksFile {
    let blocking = CursorHookEntry {
        command: command.to_string(),
        matcher: None,
        fail_closed: Some(true),
    };
    let pre_tool = CursorHookEntry {
        command: command.to_string(),
        matcher: Some(matcher.to_string()),
        fail_closed: Some(true),
    };
    let non_blocking = CursorHookEntry {
        command: command.to_string(),
        matcher: None,
        fail_closed: None,
    };
    CursorHooksFile {
        version: 1,
        hooks: CursorHooks {
            before_shell_execution: vec![blocking],
            pre_tool_use: vec![pre_tool],
            post_tool_use: vec![non_blocking.clone()],
            after_agent_thought: vec![non_blocking.clone()],
            stop: vec![non_blocking.clone()],
            session_start: vec![non_blocking],
        },
    }
}

/// Write `.cursor/hooks.json` inside `dir` with the canonical template. If
/// an existing `.cursor/hooks.json` was not written by `pocket-veto`, it is
/// backed up to `.cursor/hooks.json.pocket-veto.bak` before the overwrite.
///
/// The hook commands use `"{bin_path} hook"` so an absolute binary path can be
/// embedded when `pocket-veto` is not on PATH.
///
/// # Errors
///
/// Returns `Err` if the `.cursor` directory cannot be created, the backup
/// copy fails, or the new file cannot be written.
pub fn write_cursor_hooks(dir: &Path, bin_path: &str) -> anyhow::Result<()> {
    let cursor_dir = dir.join(".cursor");
    std::fs::create_dir_all(&cursor_dir)
        .with_context(|| format!("mkdir {}", cursor_dir.display()))?;

    let hooks_path = cursor_dir.join("hooks.json");

    // Back up an existing hooks.json unless it is already ours (idempotent
    // re-runs of `init` shouldn't pile up backups of our own file).
    if hooks_path.exists() {
        let existing = std::fs::read_to_string(&hooks_path)
            .with_context(|| format!("read existing {}", hooks_path.display()))?;
        if !is_cursor_pocket_veto_hooks(&existing) {
            let bak = cursor_dir.join("hooks.json.pocket-veto.bak");
            std::fs::write(&bak, existing.as_bytes())
                .with_context(|| format!("backup to {}", bak.display()))?;
        }
    }

    let command = format!("{bin_path} hook");
    let matcher = "Shell|Write|Task|MCP:.*";
    let file = cursor_hooks_file(&command, matcher);
    let json = serde_json::to_string_pretty(&file)
        .with_context(|| format!("serialize {}", hooks_path.display()))?;
    std::fs::write(&hooks_path, json.as_bytes())
        .with_context(|| format!("write {}", hooks_path.display()))?;
    Ok(())
}

/// Heuristic: does this `.cursor/hooks.json` content already point at
/// `pocket-veto hook`? Used to avoid stacking backups on idempotent re-runs.
fn is_cursor_pocket_veto_hooks(content: &str) -> bool {
    // Cheaper and more robust than parsing: any command in the file mentioning
    // `pocket-veto hook` means the file was written by `pocket-veto` (or the
    // user did, equivalently).
    content.contains("pocket-veto hook") || content.contains("pocket_veto hook")
}

// ---------------------------------------------------------------------------
// Claude Code `.claude/settings.json` — typed serde structs for the `hooks`
// block merged in.
// ---------------------------------------------------------------------------

/// The `hooks` object merged into `.claude/settings.json`. Keys are
/// `PascalCase` (`PreToolUse`, `PostToolUse`, `Stop`, `SessionStart`) to match
/// Claude Code's schema.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ClaudeHooks {
    pre_tool_use: Vec<ClaudeMatcher>,
    post_tool_use: Vec<ClaudeMatcher>,
    stop: Vec<ClaudeMatcher>,
    session_start: Vec<ClaudeMatcher>,
}

/// One matcher entry under a Claude hook event. `matcher` is omitted when
/// `None` (the `Stop` / `SessionStart` events have no matcher).
#[derive(Serialize)]
struct ClaudeMatcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    matcher: Option<String>,
    hooks: Vec<ClaudeHook>,
}

/// A single hook command under a Claude matcher. `kind` serializes as `type`
/// (a reserved word, hence the rename) and is always `"command"`.
#[derive(Serialize)]
struct ClaudeHook {
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
}

/// Build the [`ClaudeHooks`] block for `.claude/settings.json`. Factored out
/// so the emitted template is easy to inspect.
fn claude_hooks(bin_path: &str) -> ClaudeHooks {
    let command = format!("{bin_path} hook");
    let entry = |matcher: Option<&str>| ClaudeMatcher {
        matcher: matcher.map(String::from),
        hooks: vec![ClaudeHook {
            kind: "command",
            command: command.clone(),
        }],
    };
    ClaudeHooks {
        pre_tool_use: vec![entry(Some("Bash|Write|Edit|MCP.*"))],
        post_tool_use: vec![entry(Some(".*"))],
        stop: vec![entry(None)],
        session_start: vec![entry(None)],
    }
}

/// Merge the `PocketVeto` `hooks` block into `.claude/settings.json` inside
/// `dir`. If the file does not exist, a new one is created containing only
/// the `hooks` key. If it exists, it is parsed as JSON, the `hooks` key is
/// replaced (preserving every other top-level key such as `permissions` or
/// `env`), and the result is written back.
///
/// The hook commands use `"{bin_path} hook"`.
///
/// # Errors
///
/// Returns `Err` if the `.claude` directory cannot be created, the existing
/// file cannot be read/parsed, or the merged file cannot be written.
pub fn write_claude_hooks(dir: &Path, bin_path: &str) -> anyhow::Result<()> {
    let claude_dir = dir.join(".claude");
    std::fs::create_dir_all(&claude_dir)
        .with_context(|| format!("mkdir {}", claude_dir.display()))?;

    let settings_path = claude_dir.join("settings.json");

    // Read existing settings (if any) as a JSON object. The root is kept as a
    // `Map<String, Value>` because the user's settings file has arbitrary
    // top-level keys that must be preserved; only the merged-in `hooks` block
    // is typed.
    let mut root: Map<String, Value> = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("read existing {}", settings_path.display()))?;
        let parsed: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse existing {}", settings_path.display()))?;
        match parsed {
            Value::Object(map) => map,
            // A non-object root is unusable as a settings file; surface the
            // problem rather than silently overwriting the user's data.
            v @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)) => {
                bail!(
                    "{} is a JSON {} not an object; refusing to merge hooks into it",
                    settings_path.display(),
                    json_type_name(&v)
                )
            }
        }
    } else {
        Map::new()
    };

    let hooks =
        serde_json::to_value(claude_hooks(bin_path)).context("serialize claude hooks block")?;
    root.insert("hooks".to_string(), hooks);

    let pretty = serde_json::to_string_pretty(&Value::Object(root))
        .context("serialize merged .claude/settings.json")?;
    std::fs::write(&settings_path, pretty.as_bytes())
        .with_context(|| format!("write {}", settings_path.display()))?;
    Ok(())
}

/// Return a human-friendly JSON type name for an error message.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Write `~/.config/systemd/user/pocket-veto.service` (Linux only). Returns
/// the path of the written file. The user is responsible for running
/// `systemctl --user enable --now pocket-veto`; `init` never invokes
/// `systemctl` itself.
///
/// `home_dir` is taken explicitly (rather than read from `dirs` inside) so
/// tests can pass a `tempfile::tempdir()` path.
///
/// # Errors
///
/// Returns `Err` if the systemd user directory cannot be created or the unit
/// file cannot be written.
#[cfg(target_os = "linux")]
pub fn write_systemd_unit(home_dir: &Path, bin_path: &str) -> anyhow::Result<PathBuf> {
    let unit_dir = home_dir.join(".config").join("systemd").join("user");
    std::fs::create_dir_all(&unit_dir).with_context(|| format!("mkdir {}", unit_dir.display()))?;
    let unit_path = unit_dir.join("pocket-veto.service");
    let unit = format!(
        "[Unit]\n\
         Description=PocketVeto approval gate server\n\
         After=network.target\n\
         \n\
         [Service]\n\
         ExecStart={bin_path} serve\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );
    std::fs::write(&unit_path, unit.as_bytes())
        .with_context(|| format!("write {}", unit_path.display()))?;
    Ok(unit_path)
}

/// Write `~/Library/LaunchAgents/io.pocketveto.plist` (macOS only). Returns
/// the path of the written file. The user is responsible for running
/// `launchctl load <path>`; `init` never invokes `launchctl` itself.
///
/// `home_dir` is taken explicitly so tests can pass a `tempfile::tempdir()`.
///
/// # Errors
///
/// Returns `Err` if the LaunchAgents directory cannot be created or the plist
/// cannot be written.
#[cfg(target_os = "macos")]
pub fn write_launchd_plist(home_dir: &Path, bin_path: &str) -> anyhow::Result<PathBuf> {
    let dir = home_dir.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let plist_path = dir.join("io.pocketveto.plist");
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>io.pocketveto</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{bin_path}</string>\n\
         \t\t<string>serve</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<dict>\n\
         \t\t<key>SuccessfulExit</key>\n\
         \t\t<false/>\n\
         \t</dict>\n\
         </dict>\n\
         </plist>\n"
    );
    std::fs::write(&plist_path, plist.as_bytes())
        .with_context(|| format!("write {}", plist_path.display()))?;
    Ok(plist_path)
}

// ---------------------------------------------------------------------------
// Interactive helpers (stdin/stdout). Kept thin; logic is in the pure fns.
// ---------------------------------------------------------------------------

/// Prompt the user for any BT parameters that were not supplied as CLI flags.
/// Returns the resolved (channel, `com_port`, `adapter_addr`) tuple. Any
/// value already provided on the CLI is kept; the rest are prompted for on
/// the platform that uses them.
fn resolve_bt_params_interactively(
    bt_channel: Option<u8>,
    bt_com_port: Option<String>,
    bt_adapter_addr: Option<String>,
) -> (Option<u8>, Option<String>, Option<String>) {
    let bt_channel = if bt_channel.is_some() {
        bt_channel
    } else {
        prompt_bt_channel()
    };
    let bt_com_port = if bt_com_port.is_some() {
        bt_com_port
    } else {
        prompt_bt_com_port()
    };
    let bt_adapter_addr = if bt_adapter_addr.is_some() {
        bt_adapter_addr
    } else {
        prompt_bt_adapter_addr()
    };
    (bt_channel, bt_com_port, bt_adapter_addr)
}

#[cfg(target_os = "linux")]
fn prompt_bt_channel() -> Option<u8> {
    let s = prompt_str("RFCOMM channel (blank to skip):")?;
    s.parse::<u8>().ok()
}

#[cfg(not(target_os = "linux"))]
fn prompt_bt_channel() -> Option<u8> {
    None
}

#[cfg(target_os = "windows")]
fn prompt_bt_com_port() -> Option<String> {
    prompt_str("Windows COM port of paired phone (blank to skip):")
}

#[cfg(not(target_os = "windows"))]
fn prompt_bt_com_port() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn prompt_bt_adapter_addr() -> Option<String> {
    prompt_str("Linux Bluetooth adapter address (blank to skip):")
}

#[cfg(not(target_os = "linux"))]
fn prompt_bt_adapter_addr() -> Option<String> {
    None
}

/// Read one line from stdin, trimmed. Returns `None` on empty input or any
/// I/O failure (a closed stdin should not abort the whole init for a single
/// optional prompt).
fn prompt_str(question: &str) -> Option<String> {
    print!("{question} ");
    if std::io::stdout().flush().is_err() {
        return None;
    }
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_err() {
        return None;
    }
    let s = buf.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Read one line from stdin, trimmed. Empty stdin (EOF) returns an empty
/// string rather than an error so a closed pipe doesn't abort init.
///
/// # Errors
///
/// Returns `Err` only if stdin itself cannot be locked or read.
fn prompt(question: &str) -> anyhow::Result<String> {
    print!("{question} ");
    std::io::stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).context("read stdin")?;
    Ok(buf.trim().to_string())
}

/// Prompt for a yes/no answer. Empty input returns `default`. Recognizes
/// `y`/`yes`/`n`/`no` case-insensitively; anything else re-prompts.
fn prompt_yes_no(question: &str, default: bool) -> anyhow::Result<bool> {
    let suffix = if default { " [Y/n] " } else { " [y/N] " };
    loop {
        let answer = prompt(&format!("{question}{suffix}"))?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("  please answer 'y' or 'n'"),
        }
    }
}

/// Print platform-specific Bluetooth pairing guidance. Stub on Linux/macOS,
/// COM-port enumeration on Windows. Does not depend on `bluer` (which is
/// behind the `linux-bt` feature and unavailable in the devcontainer).
fn print_bt_guidance(opts: &InitOpts) {
    println!("Bluetooth setup");
    println!("---------------");
    #[cfg(target_os = "linux")]
    {
        println!(
            "Linux: pair your phone via your OS Bluetooth settings\n\
             (Settings -> Bluetooth). PocketVeto uses the standard SPP\n\
             service; no extra BlueZ configuration is needed beyond\n\
             `bluetoothd` running."
        );
        if let Some(addr) = &opts.bt_adapter_addr {
            println!("  adapter address: {addr}");
        }
        if let Some(ch) = opts.bt_channel {
            println!("  RFCOMM channel:  {ch}");
        }
    }
    #[cfg(target_os = "windows")]
    {
        println!(
            "Windows: pair your phone via Settings -> Bluetooth, then\n\
             pick the phone's incoming SPP COM port from the list below."
        );
        match serialport::available_ports() {
            Ok(ports) if !ports.is_empty() =>
            {
                #[allow(clippy::use_debug)]
                for p in &ports {
                    println!("  {} — {:?}", p.port_name, p.port_type);
                }
            }
            Ok(_) => println!("  (no COM ports detected)"),
            Err(e) => println!("  (could not enumerate COM ports: {e})"),
        }
        if let Some(com) = &opts.bt_com_port {
            println!("  selected COM port: {com}");
        }
    }
    #[cfg(target_os = "macos")]
    {
        println!(
            "macOS: Bluetooth is not supported in v1. The non-BT parts\n\
             (server, hook subcommand, devcontainer-host topology) work\n\
             on macOS; for Bluetooth, run the server on a Linux or\n\
             Windows host."
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        println!("Bluetooth setup is not implemented on this platform.");
    }
    println!();
}

/// Detect installed agent hosts: returns the directories where `init` should
/// consider writing hook configs (the cwd first, then the user's home dir if
/// distinct). Duplicates are removed.
fn resolve_hook_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    if let Some(home) = dirs::home_dir()
        && !dirs.contains(&home)
    {
        dirs.push(home);
    }
    dirs
}

/// Write the platform's service file (if any) and print the command the user
/// should run. Init never runs `systemctl` / `launchctl` / `reg` itself.
#[allow(clippy::unnecessary_wraps)]
fn print_service_registration(bin_path: &str) -> anyhow::Result<()> {
    println!("Service registration");
    println!("---------------------");

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().context("could not determine home directory")?;
        let unit_path = write_systemd_unit(&home, bin_path)?;
        println!("  wrote {}", unit_path.display());
        println!("  run: systemctl --user daemon-reload");
        println!("        systemctl --user enable --now pocket-veto");
    }
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().context("could not determine home directory")?;
        let plist_path = write_launchd_plist(&home, bin_path)?;
        println!("  wrote {}", plist_path.display());
        println!("  run: launchctl load {}", plist_path.display());
    }
    #[cfg(target_os = "windows")]
    {
        // No registry / scheduled-task mutation from init — just print the
        // exact commands the user can paste.
        println!("  option A (Scheduled Task, recommended):");
        println!(
            "    schtasks /Create /SC ONLOGON /TN \"PocketVeto\" \
             /TR \"\\\"{bin_path}\\\" serve\" /RL LIMITED"
        );
        println!("  option B (Registry Run key):");
        println!(
            "    reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\" \
             /V PocketVeto /T REG_SZ /D \"\\\"{bin_path}\\\" serve\" /F"
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        println!("  no service registration is implemented on this platform.");
        let _ = bin_path;
    }
    println!();
    Ok(())
}

/// Print the devcontainer snippet when devcontainer support is enabled.
fn print_devcontainer_snippet() {
    println!("Devcontainer support");
    println!("--------------------");
    println!("Server will bind 0.0.0.0:38475. In the devcontainer, set:");
    println!("  POCKET_VETO_SERVER_URL=http://host.docker.internal:38475");
    println!();
    println!("Sample devcontainer.json snippet:");
    println!(
        r#"{{
  "postCreateCommand": "curl -fsSL https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.sh | sh",
  "remoteEnv": {{ "POCKET_VETO_SERVER_URL": "http://host.docker.internal:38475" }}
}}"#
    );
    println!();
}

/// Print a final summary of the saved config.
fn print_summary(cfg: &Config, opts: &InitOpts) {
    println!();
    println!("Summary");
    println!("-------");
    println!("  server_url:           {}", cfg.server_url);
    println!("  bind_addr:            {}", cfg.bind_addr);
    println!(
        "  token:                {} ({} chars)",
        cfg.token.masked(),
        cfg.token.as_ref().len()
    );
    println!("  db_path:              {}", cfg.db_path);
    println!("  approval_timeout:     {}s", cfg.approval_timeout_seconds);
    println!("  bt_backend:           {}", cfg.bt_backend.as_str());
    if let Some(com) = &cfg.bt_com_port {
        println!("  bt_com_port:          {com}");
    }
    if let Some(addr) = &cfg.bt_adapter_addr {
        println!("  bt_adapter_addr:      {addr}");
    }
    if let Some(ch) = cfg.bt_channel {
        println!("  bt_channel:           {ch}");
    }
    println!("  devcontainer:         {}", cfg.devcontainer);
    println!("  bin_path (in hooks):  {}", opts.bin_path);
}

// ---------------------------------------------------------------------------
// Tests for the pure helpers (no stdin/stdout, no real home dir).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn cursor_hooks_file_has_exact_template_keys() {
        let file = cursor_hooks_file("pocket-veto hook", "Shell|Write|Task|MCP:.*");
        let v: Value = serde_json::to_value(file).expect("serialize");
        assert_eq!(v["version"], 1);
        let hooks = v["hooks"].as_object().expect("hooks object");
        for key in [
            "beforeShellExecution",
            "preToolUse",
            "postToolUse",
            "afterAgentThought",
            "stop",
            "sessionStart",
        ] {
            assert!(hooks.contains_key(key), "missing {key}");
        }
        // failClosed mandatory on the two blocking hooks.
        assert_eq!(hooks["beforeShellExecution"][0]["failClosed"], true);
        assert_eq!(hooks["preToolUse"][0]["failClosed"], true);
        // The non-blocking hooks have no failClosed field.
        assert!(hooks["postToolUse"][0].get("failClosed").is_none());
        // Command + matcher on preToolUse.
        assert_eq!(hooks["preToolUse"][0]["command"], "pocket-veto hook");
        assert_eq!(hooks["preToolUse"][0]["matcher"], "Shell|Write|Task|MCP:.*");
    }

    #[test]
    fn claude_hooks_has_exact_template_keys() {
        let v = serde_json::to_value(claude_hooks("pocket-veto")).expect("serialize");
        for key in ["PreToolUse", "PostToolUse", "Stop", "SessionStart"] {
            assert!(v.as_object().unwrap().contains_key(key), "missing {key}");
        }
        let pre = &v["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Bash|Write|Edit|MCP.*");
        assert_eq!(pre["hooks"][0]["type"], "command");
        assert_eq!(pre["hooks"][0]["command"], "pocket-veto hook");
        let post = &v["PostToolUse"][0];
        assert_eq!(post["matcher"], ".*");
        // Stop / SessionStart have no matcher.
        assert!(v["Stop"][0].get("matcher").is_none());
        assert!(v["SessionStart"][0].get("matcher").is_none());
    }

    #[test]
    fn pick_backend_prefers_com_port() {
        let cfg = Config {
            bt_com_port: Some("COM3".to_string()),
            ..Config::default()
        };
        assert_eq!(pick_backend(&cfg), BtBackend::Serialport);
    }

    #[test]
    fn pick_backend_prefers_adapter_addr() {
        let cfg = Config {
            bt_adapter_addr: Some("AA:BB:CC:DD:EE:FF".to_string()),
            ..Config::default()
        };
        assert_eq!(pick_backend(&cfg), BtBackend::Bluer);
    }

    #[test]
    fn is_cursor_pocket_veto_hooks_detects_our_command() {
        assert!(is_cursor_pocket_veto_hooks(
            r#"{"hooks":{"preToolUse":[{"command":"pocket-veto hook"}]}}"#
        ));
        assert!(!is_cursor_pocket_veto_hooks(
            r#"{"hooks":{"preToolUse":[{"command":"echo hi"}]}}"#
        ));
    }

    #[test]
    fn build_config_devcontainer_sets_zero_bind() {
        let opts = InitOpts {
            bin_path: "pocket-veto".to_string(),
            keep_token: false,
            devcontainer: true,
            bt_com_port: None,
            bt_adapter_addr: None,
            bt_channel: Some(3),
            existing_config: None,
        };
        let cfg = build_config(&opts);
        assert_eq!(cfg.bind_addr, "0.0.0.0:38475");
        assert!(cfg.devcontainer);
        assert_eq!(cfg.bt_channel, Some(3));
        assert_eq!(cfg.bt_backend, BtBackend::Bluer);
        assert_eq!(cfg.token.as_ref().len(), 64);
    }

    #[test]
    fn build_config_keep_token_reuses_existing() {
        let existing = Config {
            token: "known-token-123".to_string().into(),
            ..Config::default()
        };
        let opts = InitOpts {
            bin_path: "pocket-veto".to_string(),
            keep_token: true,
            devcontainer: false,
            bt_com_port: None,
            bt_adapter_addr: None,
            bt_channel: None,
            existing_config: Some(existing.clone()),
        };
        let cfg = build_config(&opts);
        assert_eq!(cfg.token.as_ref(), "known-token-123");
    }

    #[test]
    fn build_config_no_keep_token_generates_new() {
        let existing = Config {
            token: "known-token-123".to_string().into(),
            ..Config::default()
        };
        let opts = InitOpts {
            bin_path: "pocket-veto".to_string(),
            keep_token: false,
            devcontainer: false,
            bt_com_port: None,
            bt_adapter_addr: None,
            bt_channel: None,
            existing_config: Some(existing),
        };
        let cfg = build_config(&opts);
        assert_ne!(cfg.token.as_ref(), "known-token-123");
        assert_eq!(cfg.token.as_ref().len(), 64);
    }
}
