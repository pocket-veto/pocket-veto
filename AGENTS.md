# Rust Skills - Agent Instructions

> For AI coding agents working on this repository

## Default Project Settings

When creating Rust projects or Cargo.toml files, ALWAYS use:

```toml
[package]
edition = "2024"
rust-version = "1.96"

[lints.rust]
unsafe_code = "warn"

[lints.clippy]
all = "warn"
pedantic = "warn"
```

## Core Capabilities

When working on Rust code, always consult the relevant skill in `.agents/skills/` in addition to following the mandatory rules below.

### 1. Question Routing
Route Rust questions to appropriate skills:
- Ownership/borrowing → m01-ownership
- Smart pointers → m02-resource
- Error handling → m06-error-handling
- Concurrency → m07-concurrency
- Unsafe code → unsafe-checker

### 2. Code Style
Follow Rust coding guidelines:
- Use snake_case for variables and functions
- Use PascalCase for types and traits
- Use SCREAMING_SNAKE_CASE for constants
- Max line length: 100 characters
- Use `?` operator instead of `unwrap()` in library code

### 3. Error Handling
```rust
// Good: Use Result with context
fn read_config() -> Result<Config, ConfigError> {
    let content = std::fs::read_to_string("config.toml")
        .map_err(|e| ConfigError::Io(e))?;
    toml::from_str(&content)
        .map_err(|e| ConfigError::Parse(e))
}

// Avoid: unwrap() in library code
fn read_config() -> Config {
    let content = std::fs::read_to_string("config.toml").unwrap(); // Bad
    toml::from_str(&content).unwrap() // Bad
}
```

### 4. Unsafe Code
Every `unsafe` block MUST have a `// SAFETY:` comment:
```rust
// SAFETY: We checked that index < len above, so this is in bounds
unsafe { slice.get_unchecked(index) }
```

### 5. Common Error Fixes

| Error | Cause | Fix |
|-------|-------|-----|
| E0382 | Use of moved value | Clone, borrow, or use reference |
| E0597 | Lifetime too short | Extend lifetime or restructure |
| E0502 | Borrow conflict | Split borrows or use RefCell |
| E0499 | Multiple mut borrows | Restructure to single mut borrow |
| E0277 | Missing trait impl | Add trait bound or implement trait |

## Quick Reference

See the skills in `.agents/skills/` for detailed Rust guidance (`m01-ownership`, `m02-resource`, `m07-concurrency`, `m06-error-handling`, `unsafe-checker`, …).

## Skill Files

For detailed guidance, see:
- `.agents/skills/rust-router/SKILL.md` - Question routing
- `.agents/skills/coding-guidelines/SKILL.md` - Code style rules
- `.agents/skills/unsafe-checker/SKILL.md` - Unsafe code review
- `.agents/skills/m01-ownership/SKILL.md` - Ownership concepts
- `.agents/skills/m06-error-handling/SKILL.md` - Error patterns
- `.agents/skills/m07-concurrency/SKILL.md` - Concurrency patterns

## PocketVeto Idiomatic Rust Rules

The following rules govern all Rust edits in the `pocket-veto` workspace.
They are enforced by `clippy::restriction` lints together with human review.
The library crates (`pocket-veto-core`, `pocket-veto-bt`) and the binary
(`pocket-veto`) all follow these rules; crate-level exceptions are documented
in each crate's own `AGENTS.md`.

### Rule 1 — `impl Type { fn ... }` methods over free functions

Prefer methods on `impl Type` over free functions that take `&Type` as the
first argument. Free functions that operate on a single owned/borrowed value
are a code smell; they belong in the type's `impl` block.

### Rule 2 — `thiserror` for libraries, `anyhow` only at binary boundaries

Library crates (`pocket-veto-core`, `pocket-veto-bt`) return typed
`Result<_, ThisError>` via `thiserror`. `anyhow` is permitted only at the
binary boundary (`pocket-veto`). **Exception:** `pocket-veto-bt::BtTransport`
keeps `anyhow::Result` because the bridge only logs + reconnects on error and
never matches on error kind — a typed enum would add noise without enabling
any recovery branch.

### Rule 3 — No `unwrap()`/`expect()`/`panic!()`/`todo!()`/`unreachable!()` in non-test library code

Test code may use them. Enforced by `clippy::restriction` lints (`unwrap_used`,
`expect_used`, `panic`, `todo`, `unreachable`, `unwrap_in_result`).
Use `?`, `let-else`, or typed `Result` instead. If a truly-infallible case
requires `unwrap`, add
`#[allow(clippy::unwrap_used)] // SAFETY/REASON: <one-line justification>` —
but prefer eliminating it.

### Rule 4 — No stringly-typed errors

No `Result<_, String>`, `Box<dyn Error>`, or error variants carrying a
`String` message. `CoreError::ConfigIo(String)` / `Normalize(String)` /
`Protocol { message: String }` are structured variants with typed fields.

### Rule 5 — No hand-rolled reimplementation of std or an in-`Cargo.lock` crate

Use `hex`, `base64`, `uuid`, `time` formatting, length-prefix framing, JSON,
retry/backoff from their crates rather than rewriting them. If a helper is
needed by >1 crate, it lives in the lowest shared crate (usually
`pocket-veto-core`) — see Rule 15.

### Rule 6 — Public enum derives; no `#[non_exhaustive]` yet

Public enums derive `Debug, Clone`; wire enums additionally derive
`serde::{Serialize, Deserialize}`. Do NOT add `#[non_exhaustive]` — every
consumer is currently in-tree, so exhaustive `match` arms give compile errors
when a variant is added (forcing you to handle it), which is safer than
`#[non_exhaustive]`'s silent wildcard catch.

### Rule 7 — Domain identifiers are newtypes

`AgentId`, `ApprovalId`, `SessionId`, `ComPort`, `RfcommChannel`,
`TimestampMs`, `Token` are newtypes, not bare `String`/`i64`. They are
wire-transparent via `#[serde(transparent)]` and derive
`Debug, Clone, PartialEq, Eq, Hash` (+ `Serialize, Deserialize`).

### Rule 8 — Prefer `let-else` and `let-chains` (edition 2024)

Use `let Some(x) = opt else { return ... };` and `let-chains` over nested
`if let Some(...)` / early-return ladders.

### Rule 9 — `std::sync::LazyLock` / `OnceLock` over `lazy_static!` / `once_cell::Lazy` / `static mut`

Edition 2024 / Rust 1.96 std has these; do not pull in `lazy_static` or
`once_cell` for new code.

### Rule 10 — Every `unsafe` block has `// SAFETY:`; every public `unsafe fn` has a `# Safety` doc section

Enforced by `clippy::undocumented_unsafe_blocks` + `clippy::missing_safety_doc`.
`unsafe_code` is `deny` at the workspace level.

### Rule 11 — Traits with associated types / generics over `Box<dyn Trait>`; native AFIT over `async_trait`

Where dispatch is static, use generics/associated types. For new async traits,
use native `async fn` in traits (AFIT, stable on 1.96) — do not pull in
`async-trait`. Add explicit `+ Send` bounds on returned futures where the
consumer spawns the future.

### Rule 12 — `#[must_use]` on constructors and `Result`-returning public fns

Annotate constructors (`fn new(...) -> Self`) and public functions returning `Result` or `Option` with `#[must_use]`, so a caller who drops the value gets a compile-time warning. This catches silently-dropped errors and unused builders. For `Result`-returning fns it complements Rule 3's no-`unwrap` rule: the type system reminds the caller to handle the error rather than ignore it.

### Rule 13 — Subcommand-style CLIs use `clap` derive + `impl SubcommandArgs { async fn run(...) }`; `main` returns `Result` + `Termination`

No hand-rolled `match` dispatch with `process::exit`. `main` returns
`Result<(), ExitCode>` (or `anyhow::Result<()>` with a `Termination` impl) and
stays under ~30 lines.

### Rule 14 — Edition-2024 module layout: `foo.rs` (with submodules in `foo/`), never `foo/mod.rs`

The `mod.rs` style is the pre-2018 convention, deprecated for new code.
`clippy::mod_module_files = "deny"` enforces it in the workspace lints.

### Rule 15 — No cross-crate code duplication

If a function, struct, constant, or helper is needed by more than one crate,
it lives in the lowest crate on the dependency graph that all consumers
already depend on (usually `pocket-veto-core`) and is re-exported by
dependents — never copy-pasted into two crates. Within a single crate, dedup
via a shared private helper / trait / macro rather than repeating the block.

### Before you claim done (pre-submit checklist)

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace --all-targets` passes
- [ ] `cargo doc --no-deps --workspace --document-private-items` builds with no warnings
- [ ] No new `unwrap`/`expect`/`panic`/`todo`/`unreachable` in non-test code
- [ ] Every new `unsafe` block has `// SAFETY:`; new public `unsafe fn` has `# Safety` doc
- [ ] New public enums derive `Debug, Clone` (+ serde if on wire); do NOT add `#[non_exhaustive]` (see Rule 6)
- [ ] New fallible public fns return typed `Result` (`thiserror` in libs, `anyhow` at binary boundary) and are `#[must_use]`
- [ ] New domain identifiers are newtypes
- [ ] No `Result<_, String>` / stringly-typed error variants
- [ ] No hand-rolled reimplementation of std or an in-workspace crate
- [ ] No cross-crate duplication: if a helper is needed by >1 crate, it lives in the lowest shared crate (usually `pocket-veto-core`) and is re-exported — never copy-pasted across crates (see Rule 15)
- [ ] If you touched `pocket-veto-core/src/protocol.rs`, you mirrored it in `android/.../Protocol.kt` and added a `pocket-veto-bt` mock round-trip test
- [ ] Paste the command output; do not claim "should pass"
