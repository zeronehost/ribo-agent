# Ribo Agent

> A modular Rust workspace for building AI agents with **tool calling**, **multi-model** support, and **event-driven workflows**.

Ribo Agent ships two binaries — `ribo` (a streaming command-line REPL) and `ribo-tui` (a full-screen terminal TUI) — built on a set of single-responsibility crates: a provider abstraction, a tool system, an event-driven agent loop, a session container, and configuration assembly.

- License: [Mulan Permissive Software License, v2 (MulanPSL-2.0)](LICENSE)
- Toolchain: Rust **1.85.0+**, `edition = 2024`

---

## Features

- **Two interfaces**: `ribo` is a streaming REPL; `ribo-tui` is a full-screen TUI (left status sidebar + right dialog / loading / input panes + help overlay); tool calls are inlined in the chat stream, and the sidebar is hidden when terminal width < 60.
- **Multi-model & multi-provider**: built-in `openai-chat`, `openai-responses`, and `anthropic` adapters plus a `mock` simulator, with cross-provider fallback chains.
- **Tool calling**: 9 built-in tools (echo, calculator, current time, and a six-piece filesystem family), with the filesystem tools confined to a sandbox root.
- **Gateway resilience**: request retries (exponential backoff + jitter), first-token timeout, cross-provider fallback, circuit breaking, and admission control.
- **Sessions & context**: session persistence, configurable context compaction (summary / sliding window), system prompt and environment injection.
- **Skill system**: loads `SKILL.md` skills from scan roots; load diagnostics are non-fatal.
- **Layered configuration**: six independent layers (provider / retry / compression / session / tools / harness); falls back to Mock mode when unconfigured in debug builds (release builds abort with `config.no_providers_in_release` instead). Also supports two trust domains (Project / Global) and two trust tiers (documents trusted by default, code requires explicit grant); data outside the project is exposed read-only via global `[fs.mounts]`.

---

## Architecture

| Crate | Responsibility |
| :--- | :--- |
| `ra-core` | Unified error type and conversation message model (user / assistant / tool / usage). |
| `ra-provider` | Provider abstraction, model catalog, gateway (fallback / circuit-break / admission), and OpenAI-compatible adapters. |
| `ra-tools` | `AgentTool` declaration, registry, schema validation, circuit-break orchestration, and built-in tools. |
| `ra-agent` | Event-driven agent loop, session, abort / queue. |
| `ra-harness` | Session container: persistence, context compaction, system prompt, and skill loading. |
| `ra-cli` | Command-line interaction layer: REPL, streaming render, layered config (binary `ribo`). |
| `ra-tui` | Terminal TUI: five-zone full-screen interaction (binary `ribo-tui`). |

The workspace is organized in `Cargo.toml` with `resolver = "3"`; members are `crates/*` and `internals/git-hook`.

---

## Requirements

- Rust toolchain **1.85.0** or newer (`edition 2024`).
- The project is distributed as an executable application; committing `Cargo.lock` is recommended.

---

## Download Prebuilt Binaries

No need to build from source: [GitHub Releases](https://github.com/zeronehost/ribo-agent/releases) provides prebuilt binaries for Windows (x64), macOS (x64 / arm64), and Linux (x64 / arm64, statically linked with musl). Download the archive for your platform, extract it, and put `ribo` / `ribo-tui` on your `PATH`.

---

## Quick Start

```bash
# 1. Clone
git clone https://gitee.com/zeronehost/ribo-agent.git
cd ribo-agent

# 2. Build all crates (produces the ribo and ribo-tui binaries)
cargo build --workspace

# 3. Run in Mock mode with no keys required
cargo run -p ra-cli        # command-line REPL
cargo run -p ra-tui        # full-screen terminal TUI
```

> **Mock mode**: when `--config` is not given and no `ribo.toml` exists in the current directory, the entire request path falls back to a local simulator (`mock-model`), so you can try the interaction and tool flow with zero configuration (`cargo run` is a debug build). Release builds forbid this fallback: missing providers or placeholder configs abort with `config.no_providers_in_release` / `config.placeholder_in_release`.

---

## Command-line Flags

Both binaries share the same `clap`-parsed flags:

| Flag | Description |
| :--- | :--- |
| `-c, --config <PATH>` | Config file path. When omitted, `ribo.toml` in the current directory is auto-detected; if absent, Mock mode is entered (debug builds; release builds abort without providers). |
| `--root <PATH>` | Override the workspace root (`harness` layer `root`, defaults to the process cwd). |
| `--global-dir <PATH>` | Override the global domain directory (default: platform config dir + `/ribo`). |
| `--allow-duplicate-plugin-id` | Allow cross-source duplicate plugin IDs (first declaration wins); rejected fatally by default. |

One-shot subcommands (`auth` / `trust` / `config` / `plugins list`) are documented in "One-shot Subcommands" below.

---

## Interactive Commands

Inside the REPL / TUI, a line starting with `/` is treated as a command; any other line is sent to the model as a user message.

| Command | Alias | Target | Description |
| :--- | :--- | :--- | :--- |
| `/help` | `/?` | CLI + TUI | Show the command list. |
| `/stop` | `/cancel` | CLI + TUI | Cancel the in-flight turn. |
| `/skills` | — | CLI + TUI | List loaded skills and diagnostics. |
| `/sessions [name]` | — | CLI + TUI | List session keys (→ marks current); switch to or create a session when named. |
| `/status` | — | CLI only | Status summary (session / mode / in-flight / token / context triplet / warnings); always shown in the TUI sidebar. |
| `/history [n]` | — | CLI only | Show the last `n` history records; fixed default of 10; TUI chat area is the full history. |
| `/exit` | `/quit` | CLI + TUI | Quit. |

---

## Configuration

Configuration uses a **multi-layer schema**; each layer is independent with zero cross-layer coupling. `[routing].default` is an explicit `(provider, model)` entry: one provider can declare multiple models via its `models` array, with automatic fallback through the implicit same-provider chain and `[[routing.fallback]]` rules.

| Layer | Key fields | Defaults |
| :--- | :--- | :--- |
| `provider` / `routing` | `[[providers]]` (adapter / base_url / api_key / `[[providers.models]]`), `[routing].default`, `[[routing.fallback]]` | default = mock/mock-model, no providers, no rules |
| `retry` | `max_retries`, `initial_backoff_ms`, `jitter_ratio` | 3 / 500ms / 0.25 |
| `compression` | `enabled`, `trigger_ratio`, `keep_recent_turns`, `summarizer_model`, `max_summary_chars`, `hysteresis_ratio`, `cooldown_turns`, `enable_incremental`, `incremental_rebuild_every`, `max_summary_input_tokens` | off / 0.75 / 4 / None / 2048 / 1.10 / 2 / true / 8 / 0 |
| `session` | `default_key`, `history_lines` | `"default"` / 10 |
| `tools` | `builtin_enabled` | true / sandbox is always harness.root (no `fs_root` setting) |
| `harness` | `root`, `include_environment` | `"."` / true (skills fixed at `.ribo/skills/`) |

- **Secrets never hit disk**: `api_key` is always written as a `"$ENV_NAME"` placeholder, resolved from the environment at runtime; an unset variable causes that provider to be skipped.
- **Degradation semantics**:
  - Template left in placeholder state (`your-*` / `example.com`) → whole stack falls back to Mock.
  - A missing `$ENV` key → local degradation; fallback targets referencing it are dropped, and if `default` becomes dangling the whole stack falls back to Mock.
  - A single layer failing to parse (except ignored unknown fields) → that layer keeps its defaults and prints a warning.
- **Config selection**: an explicit `--config` wins; when omitted, `ribo.toml` is loaded only if present, otherwise fully-defaulted Mock mode is used (debug builds; release builds abort without providers).

A complete, copyable template is [`ribo_example.toml`](ribo_example.toml): copy it to `ribo.toml` and fill in real endpoints and keys.

```toml
# ribo.toml excerpt
[routing]
default = { provider = "chat-primary", model = "your-model-name" }

[[providers]]
provider_id = "chat-primary"
adapter     = "openai-chat"
base_url    = "https://your-endpoint.example.com"
api_key     = "$OPENAI_API_KEY"

[[providers.models]]
id             = "your-model-name"
context_window = 131072       # optional, default 8192

[[providers.models]]          # same-provider fallback model
id             = "your-model-lite"

[[routing.fallback]]            # rules are matched in declaration order
match = { provider = "chat-primary" }
targets = [{ provider = "anthropic-backup" }]
```

---

## Two Trust Domains and Two Trust Tiers

Ribo separates configuration and resources into two **trust domains** (Project / Global) plus read-only mounts, with two distinct trust tiers:

| Domain | Location | Contents |
| :--- | :--- | :--- |
| **Project** | `harness.root` | The project's own `ribo.toml`, `skills/`, `AGENTS.md`, `.wasm` plugins |
| **Global** | Platform config dir + `/ribo` (override with `--global-dir`) | `settings.toml`, `auth.toml`, `trusted_projects.toml`, cross-project resources |
| **Read-only mounts** | Absolute paths in global `[fs.mounts]` | Read shared data outside the project (read-only) |

**Two trust tiers** (deliberately kept separate):

- **Config trust** (on by default, no record needed): **document-level** resources such as `ribo.toml` / `AGENTS.md` / `skills/`. Loading them only adds context to the model and executes no code, so trusting them by default is safe.
- **Code trust** (off by default, must be granted explicitly): **executable** resources such as `.wasm` plugins. A Project's plugins are not loaded on first encounter; instead the startup banner prints:

  ```text
  WARN project `/path/to/proj` has plugins but is not code-trusted.
       Run `ribo trust add /path/to/proj` after reviewing the plugins.
       Skills and AGENTS.md are still loaded.
  ```

  After reviewing the plugins, run `ribo trust add <path>` to grant trust; use `ribo trust list` to inspect and `ribo trust remove <path>` to revoke. Trust records live in the global domain's `trusted_projects.toml` (hand-written comments preserved). The TUI shows a y/n confirmation at startup when an untrusted project declares plugins.

### Read-only mounts and external resources

Global `settings.toml [fs.mounts]` declares read-only path aliases;
models read outside data via `read("datasets://x.jpg")`.
Global-only, absolute paths outside the work directory, writes rejected.
See [configuration](docs/src/configuration.md).

### One-shot Subcommands

These run before REPL assembly, print their output, and exit; with no subcommand, behavior is unchanged (the REPL starts directly):

```bash
ribo auth login <provider> [--api-key KEY]   # set a credential (KEY omitted → hidden TTY input)
ribo auth status                             # list configured providers (keys hidden)
ribo auth logout <provider>                  # remove a credential
ribo trust add <path> [--note NOTE]          # grant project code trust
ribo trust remove <path>                     # revoke code trust
ribo trust list                              # list trust records
ribo config --show-origin                    # show each config field's source domain
ribo config --explain                        # same, with conflict explanation
ribo plugins list                            # list plugin entries and domains in two sections
```

Credential and trust files are created on first use; no manual migration is needed.

---

## Provider Adapters

| Adapter | Protocol | Notes |
| :--- | :--- | :--- |
| `openai-chat` | OpenAI Chat Completions | Classic `/chat/completions` streaming SSE. |
| `openai-responses` | OpenAI Responses API | Newer responses endpoint. |
| `anthropic` | Anthropic Messages | Includes SSE parsing. |
| `mock` | Local simulation | Enabled by default when unconfigured; for development and demos. |

The gateway layer provides retries, first-token timeout, cross-provider fallback, circuit breaking, and admission control; the model catalog (`ra-provider::model`) manages routable models.

---

## Built-in Tools

Registered via `ra_tools::builtin::register_builtin`, **9** in total:

| Tool | Description | Sandbox |
| :--- | :--- | :--- |
| `echo` | Echo the input. | — |
| `calculator` | Expression evaluation (`evalexpr`). | — |
| `current_time` | Current time (`jiff`). | — |
| `read` | Read a file's contents. | sandbox / read-only mounts |
| `write` | Write a file. | sandbox |
| `edit` | Edit a file in place. | sandbox |
| `ls` | List a directory. | sandbox / read-only mounts |
| `glob` | Glob-match paths. | sandbox / read-only mounts |
| `grep` | Search file contents. | sandbox / read-only mounts |

The filesystem tools (read / write / edit / ls / glob / grep) share a single sandbox root `harness.root`; paths outside the project are available only through read-only `[fs.mounts]` declared in global settings.

---

## Skill System

A skill is a `SKILL.md` file with frontmatter (parsed by hand, without `serde_yaml`):

```markdown
---
name: example
description: an example skill
---
skill body……
```

- Scanned from fixed directories `<harness.root>/.ribo/skills/` (project) and `skills/` in the global domain; project skills take priority. The old `harness.skills_roots` setting has been removed.
- Load diagnostics (missing `SKILL.md`, read failure, frontmatter parse failure, etc.) are **non-fatal**: the loader returns the usable set plus diagnostics, and the caller logs them and continues assembly.
- Use the `/skills` command at runtime to inspect loaded skills and diagnostics.

---

## Development & Quality Gates

Development conventions live in [`AGENTS.md`](AGENTS.md). The gates that must pass before committing:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

- Clippy runs with `-D warnings`; warnings are treated as errors.
- Tests that touch external services are marked `#[ignore]`; `real_api.rs` must be run manually with keys present.
- Logging uses `tracing` via fully-qualified paths (`tracing::info!(...)`), **English only**, and must not record sensitive information.
- Detailed design documents live under `internals/docs`.

---

## Reference Projects

Ribo Agent was not designed from scratch; the projects below are the primary **blueprints** and **reference material** used during architecture.

### Target Architecture Blueprint

- **`pi-rust`** (pi-ai / pi-agent / pi-harness / pi-tools / pi-cli / pi-tui / rpi-extensions, etc.): the **target architecture blueprint**. This workspace's crate layering (core / provider / tools / agent / harness / cli / tui) and core patterns — the event-driven agent loop, session container, and gateway fallback / circuit-break / admission — were all ported from it.

> Usage principle: for any given capability, prefer aligning with the `pi-rust` blueprint form; historical repos fill gaps the blueprint does not cover (Windows adaptation, audit / throttling, config layering, etc.), and are refactored — not copied wholesale — per the rename/refactor plan.

---

## License

This project is licensed under the [Mulan Permissive Software License, v2 (MulanPSL-2.0)](LICENSE).
