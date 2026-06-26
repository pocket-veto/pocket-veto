#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing,
    clippy::missing_docs_in_private_items,
    clippy::tests_outside_test_module,
    clippy::wildcard_enum_match_arm,
    clippy::ref_patterns,
    clippy::print_stderr,
    clippy::mem_forget
)]
//! Integration tests for the `pocket-veto init` subcommand's pure helpers.
//!
//! `init::run` is interactive (stdin prompts), so the tests here exercise the
//! testable pure functions it delegates to: [`write_cursor_hooks`],
//! [`write_claude_hooks`], [`build_config`], and (on Linux)
//! [`write_systemd_unit`]. Each test uses a [`tempfile::tempdir`] as the
//! "home" or "project" root so no real user files are touched.

use std::fs;

use pocket_veto::init::{InitOpts, build_config, write_claude_hooks, write_cursor_hooks};
use pocket_veto_core::config::{BtBackend, Config};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Cursor .cursor/hooks.json
// ---------------------------------------------------------------------------

#[test]
fn write_cursor_hooks_creates_file_with_exact_template() {
    let tmp = tempfile::tempdir().expect("tempdir");

    write_cursor_hooks(tmp.path(), "pocket-veto").expect("write");

    let hooks_path = tmp.path().join(".cursor").join("hooks.json");
    let raw = fs::read_to_string(&hooks_path).expect("read hooks.json");
    let v: Value = serde_json::from_str(&raw).expect("parse hooks.json");

    assert_eq!(v["version"], 1, "version must be 1");

    let hooks = v["hooks"].as_object().expect("hooks is an object");
    // All six event keys must be present.
    for key in [
        "beforeShellExecution",
        "preToolUse",
        "postToolUse",
        "afterAgentThought",
        "stop",
        "sessionStart",
    ] {
        assert!(hooks.contains_key(key), "missing hooks key: {key}");
    }

    // failClosed is mandatory on the two blocking hooks.
    assert_eq!(
        hooks["beforeShellExecution"][0]["failClosed"],
        Value::Bool(true),
        "beforeShellExecution must be fail-closed"
    );
    assert_eq!(
        hooks["preToolUse"][0]["failClosed"],
        Value::Bool(true),
        "preToolUse must be fail-closed"
    );
    // The non-blocking hooks do not set failClosed.
    assert!(
        hooks["postToolUse"][0].get("failClosed").is_none(),
        "postToolUse should not set failClosed"
    );

    // Command + matcher on preToolUse.
    assert_eq!(
        hooks["preToolUse"][0]["command"], "pocket-veto hook",
        "preToolUse command"
    );
    assert_eq!(
        hooks["preToolUse"][0]["matcher"], "Shell|Write|Task|MCP:.*",
        "preToolUse matcher"
    );

    // Every command in every hook must be `pocket-veto hook`.
    for (_event, arr) in hooks {
        for entry in arr.as_array().expect("hook entry is array") {
            assert_eq!(entry["command"], "pocket-veto hook", "command in {raw}");
        }
    }
}

#[test]
fn write_cursor_hooks_backs_up_existing_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cursor_dir = tmp.path().join(".cursor");
    fs::create_dir_all(&cursor_dir).expect("mkdir .cursor");
    let hooks_path = cursor_dir.join("hooks.json");
    let original = r#"{"version": 1, "hooks": {"preToolUse": [{"command": "echo hi"}]}}"#;
    fs::write(&hooks_path, original).expect("seed hooks.json");

    write_cursor_hooks(tmp.path(), "pocket-veto").expect("write");

    let bak = cursor_dir.join("hooks.json.pocket-veto.bak");
    assert!(bak.exists(), "backup file should exist");
    assert_eq!(
        fs::read_to_string(&bak).expect("read bak"),
        original,
        "backup should contain the old content"
    );
    // The new hooks.json should be ours, not the seed.
    let new = fs::read_to_string(&hooks_path).expect("read new hooks.json");
    assert!(
        new.contains("pocket-veto hook"),
        "new hooks.json should point at pocket-veto"
    );
    assert!(
        !new.contains("echo hi"),
        "new hooks.json should not contain the old command"
    );
}

#[test]
fn write_cursor_hooks_idempotent_no_backup_of_own_file() {
    // A second run against an already-pocket-veto hooks.json should NOT
    // create another backup (idempotent re-runs shouldn't pile up .bak
    // files of our own config).
    let tmp = tempfile::tempdir().expect("tempdir");
    write_cursor_hooks(tmp.path(), "pocket-veto").expect("first write");
    write_cursor_hooks(tmp.path(), "pocket-veto").expect("second write");

    let bak = tmp
        .path()
        .join(".cursor")
        .join("hooks.json.pocket-veto.bak");
    assert!(
        !bak.exists(),
        "idempotent re-run should not back up our own file"
    );
}

#[test]
fn cursor_template_uses_custom_bin_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let custom = "/usr/local/bin/pocket-veto";
    write_cursor_hooks(tmp.path(), custom).expect("write");

    let raw =
        fs::read_to_string(tmp.path().join(".cursor").join("hooks.json")).expect("read hooks.json");
    let v: Value = serde_json::from_str(&raw).expect("parse");
    assert_eq!(
        v["hooks"]["preToolUse"][0]["command"],
        format!("{custom} hook"),
        "command should embed the custom bin path"
    );
}

// ---------------------------------------------------------------------------
// Claude Code .claude/settings.json
// ---------------------------------------------------------------------------

#[test]
fn write_claude_hooks_merges_into_existing_settings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude_dir = tmp.path().join(".claude");
    fs::create_dir_all(&claude_dir).expect("mkdir .claude");
    let settings_path = claude_dir.join("settings.json");
    // Pre-existing settings with a `permissions` block that must be preserved.
    let existing = r#"{"permissions": {"allow": ["Bash"]}}"#;
    fs::write(&settings_path, existing).expect("seed settings.json");

    write_claude_hooks(tmp.path(), "pocket-veto").expect("write");

    let raw = fs::read_to_string(&settings_path).expect("read settings.json");
    let v: Value = serde_json::from_str(&raw).expect("parse settings.json");

    // permissions preserved.
    assert_eq!(
        v["permissions"]["allow"][0], "Bash",
        "existing permissions must be preserved"
    );

    // hooks added with all four event keys.
    let hooks = v["hooks"].as_object().expect("hooks object");
    for key in ["PreToolUse", "PostToolUse", "Stop", "SessionStart"] {
        assert!(hooks.contains_key(key), "missing hooks key: {key}");
    }

    // Spot-check the PreToolUse matcher + nested hook shape.
    let pre = &hooks["PreToolUse"][0];
    assert_eq!(pre["matcher"], "Bash|Write|Edit|MCP.*");
    assert_eq!(pre["hooks"][0]["type"], "command");
    assert_eq!(pre["hooks"][0]["command"], "pocket-veto hook");

    // PostToolUse matcher is ".*".
    assert_eq!(hooks["PostToolUse"][0]["matcher"], ".*");

    // Stop and SessionStart have no matcher.
    assert!(hooks["Stop"][0].get("matcher").is_none());
    assert!(hooks["SessionStart"][0].get("matcher").is_none());
}

#[test]
fn write_claude_hooks_creates_new_file_when_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_claude_hooks(tmp.path(), "pocket-veto").expect("write");

    let settings_path = tmp.path().join(".claude").join("settings.json");
    assert!(settings_path.exists(), "settings.json should be created");

    let v: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).expect("read")).expect("parse");
    let hooks = v["hooks"].as_object().expect("hooks object");
    for key in ["PreToolUse", "PostToolUse", "Stop", "SessionStart"] {
        assert!(hooks.contains_key(key), "missing hooks key: {key}");
    }
    assert_eq!(
        hooks["PreToolUse"][0]["hooks"][0]["command"],
        "pocket-veto hook"
    );
}

#[test]
fn write_claude_hooks_uses_custom_bin_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let custom = "/opt/pocket-veto/bin/pocket-veto";
    write_claude_hooks(tmp.path(), custom).expect("write");

    let v: Value = serde_json::from_str(
        &fs::read_to_string(tmp.path().join(".claude").join("settings.json")).expect("read"),
    )
    .expect("parse");
    assert_eq!(
        v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        format!("{custom} hook"),
    );
}

#[test]
fn write_claude_hooks_preserves_multiple_existing_keys() {
    // A more realistic pre-existing settings.json with several top-level
    // keys; all should survive the merge.
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude_dir = tmp.path().join(".claude");
    fs::create_dir_all(&claude_dir).expect("mkdir .claude");
    let existing = r#"{
        "permissions": {"allow": ["Bash", "Read"]},
        "env": {"MY_VAR": "1"},
        "model": "claude-sonnet"
    }"#;
    fs::write(claude_dir.join("settings.json"), existing).expect("seed");

    write_claude_hooks(tmp.path(), "pocket-veto").expect("write");

    let v: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).expect("read"))
            .expect("parse");
    assert_eq!(v["permissions"]["allow"][1], "Read");
    assert_eq!(v["env"]["MY_VAR"], "1");
    assert_eq!(v["model"], "claude-sonnet");
    assert!(v["hooks"]["PreToolUse"].is_array());
}

// ---------------------------------------------------------------------------
// build_config
// ---------------------------------------------------------------------------

fn base_opts() -> InitOpts {
    InitOpts {
        bin_path: "pocket-veto".to_string(),
        keep_token: false,
        devcontainer: false,
        bt_com_port: None,
        bt_adapter_addr: None,
        bt_channel: None,
        existing_config: None,
    }
}

#[test]
fn build_config_uses_defaults_and_opts() {
    let mut opts = base_opts();
    opts.devcontainer = true;
    opts.bt_channel = Some(5);

    let cfg = build_config(&opts);

    assert_eq!(cfg.bind_addr, "0.0.0.0:38475", "devcontainer bind");
    assert!(cfg.devcontainer, "devcontainer flag set");
    assert_eq!(cfg.bt_channel, Some(5), "bt_channel passed through");
    // A channel forces the Bluer backend.
    assert_eq!(cfg.bt_backend, BtBackend::Bluer);
    // Fresh token, 64 hex chars.
    assert_eq!(cfg.token.as_ref().len(), 64, "token is 64 hex chars");
    assert!(
        cfg.token.as_ref().chars().all(|c| c.is_ascii_hexdigit()),
        "token is hex"
    );
    // Default server URL untouched.
    assert_eq!(cfg.server_url, "http://127.0.0.1:38475");
    assert_eq!(cfg.approval_timeout_seconds, 300);
}

#[test]
fn build_config_com_port_forces_serialport_backend() {
    let mut opts = base_opts();
    opts.bt_com_port = Some("COM3".to_string());

    let cfg = build_config(&opts);
    assert_eq!(cfg.bt_com_port.as_deref(), Some("COM3"));
    assert_eq!(cfg.bt_backend, BtBackend::Serialport);
}

#[test]
fn build_config_keep_token_reuses_existing() {
    let existing = Config {
        token: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            .to_string()
            .into(),
        ..Config::default()
    };
    let mut opts = base_opts();
    opts.keep_token = true;
    opts.existing_config = Some(existing.clone());

    let cfg = build_config(&opts);
    assert_eq!(cfg.token, existing.token, "keep_token must reuse the token");
}

#[test]
fn build_config_generates_new_token_when_not_keep() {
    let existing = Config {
        token: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            .to_string()
            .into(),
        ..Config::default()
    };
    let mut opts = base_opts();
    opts.keep_token = false;
    opts.existing_config = Some(existing);

    let cfg = build_config(&opts);
    assert_ne!(
        cfg.token.as_ref(),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "a new token should be generated when keep_token is false"
    );
    assert_eq!(cfg.token.as_ref().len(), 64);
}

#[test]
fn build_config_keep_token_with_no_existing_still_has_token() {
    // keep_token=true but no existing config -> falls back to the freshly
    // generated default token (does not panic, does not produce empty).
    let mut opts = base_opts();
    opts.keep_token = true;
    opts.existing_config = None;

    let cfg = build_config(&opts);
    assert_eq!(cfg.token.as_ref().len(), 64);
    assert!(cfg.token.as_ref().chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn build_config_non_devcontainer_keeps_loopback_bind() {
    let opts = base_opts();
    let cfg = build_config(&opts);
    assert_eq!(cfg.bind_addr, "127.0.0.1:38475");
    assert!(!cfg.devcontainer);
}

// ---------------------------------------------------------------------------
// Service registration (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn write_systemd_unit_creates_file() {
    use pocket_veto::init::write_systemd_unit;

    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = "/usr/local/bin/pocket-veto";
    let unit_path = write_systemd_unit(tmp.path(), bin).expect("write unit");

    let expected = tmp
        .path()
        .join(".config")
        .join("systemd")
        .join("user")
        .join("pocket-veto.service");
    assert_eq!(unit_path, expected, "unit path");
    assert!(unit_path.exists(), "unit file exists");

    let content = fs::read_to_string(&unit_path).expect("read unit");
    assert!(
        content.contains("ExecStart=/usr/local/bin/pocket-veto serve"),
        "unit must contain the ExecStart line with the bin path: {content}"
    );
    assert!(
        content.contains("WantedBy=default.target"),
        "unit must contain the Install section"
    );
    assert!(
        content.contains("Restart=on-failure"),
        "unit must restart on failure"
    );
}

// ---------------------------------------------------------------------------
// LaunchAgent plist (macOS only) — compiled but not run on this CI host.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn write_launchd_plist_creates_file() {
    use pocket_veto::init::write_launchd_plist;

    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = "/usr/local/bin/pocket-veto";
    let plist_path = write_launchd_plist(tmp.path(), bin).expect("write plist");

    let expected = tmp
        .path()
        .join("Library")
        .join("LaunchAgents")
        .join("io.pocketveto.plist");
    assert_eq!(plist_path, expected, "plist path");
    assert!(plist_path.exists(), "plist file exists");

    let content = fs::read_to_string(&plist_path).expect("read plist");
    assert!(content.contains("<string>io.pocketveto</string>"), "Label");
    assert!(
        content.contains(&format!("<string>{bin}</string>")),
        "bin path"
    );
    assert!(content.contains("<string>serve</string>"), "serve arg");
}

// ---------------------------------------------------------------------------
// Config round-trip through pocket_veto_core::config (sanity check that build_config
// output is saveable and loadable).
// ---------------------------------------------------------------------------

#[test]
fn build_config_output_roundtrips_through_toml() {
    let mut opts = base_opts();
    opts.devcontainer = true;
    opts.bt_channel = Some(7);
    opts.bt_adapter_addr = Some("AA:BB:CC:DD:EE:FF".to_string());

    let cfg = build_config(&opts);
    let toml_str = toml::to_string_pretty(&cfg).expect("ser");
    let back: Config = toml::from_str(&toml_str).expect("de");
    assert_eq!(back, cfg, "round-trip preserves all fields");
}
