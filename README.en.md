# Ribo Agent

> A modular Rust workspace for building AI agents with **tool calling**, **multi-model** support, and **event-driven workflows**.

Ribo Agent ships two binaries — `ribo` (a streaming command-line REPL) and `ribo-tui` (a full-screen terminal TUI) — built on a set of single-responsibility crates: a provider abstraction, a tool system, an event-driven agent loop, a session container, and configuration assembly.

- License: [Mulan Permissive Software License, v2 (MulanPSL-2.0)](LICENSE)
- Toolchain: Rust **1.85.0+**, `edition = 2024`

---

## Features

- **Two interfaces**: `ribo` is a streaming REPL; `ribo-tui` is a five-zone full-screen TUI (input, output, status line, tool status, help overlay).
- **Multi-model & multi-provider**: built-in `openai-chat`, `openai-responses`, and `anthropic` adapters plus a `mock` simulator, with cross-provider fallback chains.
- **Tool calling**: 9 built-in tools (echo, calculator, current time, and a six-piece filesystem family), with the filesystem tools confined to a sandbox root.
- **Gateway resilience**: request retries (exponential backoff + jitter), first-token timeout, cross-provider fallback, circuit breaking, and admission control.
- **Sessions & context**: session persistence, configurable context compaction (summary / sliding window), system prompt and environment injection.
- **Skill system**: loads `SKILL.md` skills from scan roots; load diagnostics are non-fatal.
- **Layered configuration**: six independent layers (provider / retry / compression / session / tools / harness); falls back to Mock mode when unconfigured.

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

> **Mock mode**: when `--config` is not given and no `ribo.toml` exists in the current directory, the entire request path falls back to a local simulator (`mock-model`), so you can try the interaction and tool flow with zero configuration.

---

## Command-line Flags

Both binaries share the same `clap`-parsed flags:

| Flag | Description |
| :--- | :--- |
| `-c, --config <PATH>` | Config file path. When omitted, `ribo.toml` in the current directory is auto-detected; if absent, Mock mode is entered. |
| `--root <PATH>` | Override the workspace root (`harness` layer `root`, defaults to the process cwd). |

---

## Interactive Commands

Inside the REPL / TUI, a line starting with `/` is treated as a command; any other line is sent to the model as a user message.

| Command | Alias | Description |
| :--- | :--- | :--- |
| `/help` | `/?` | Show the command list. |
| `/new [name]` | — | Rotate the session key; auto-rotates when `name` is omitted. |
| `/stop` | `/cancel` | Cancel the in-flight turn. |
| `/skills` | — | List loaded skills and diagnostics. |
| `/sessions` | — | List session keys. |
| `/status` | — | Status summary (session / mode / in-flight). |
| `/history [n]` | — | Show the last `n` transcripts; defaults to 10. |
| `/exit` | `/quit` | Quit. |

---

## Configuration

Configuration uses a **six-layer schema**; each layer is independent with zero cross-layer coupling. The root-level field `query_model` is the gateway query entry model.

| Layer | Key fields | Defaults |
| :--- | :--- | :--- |
| `provider` | `query_model`, `[[providers]]` (adapter / base_url / api_key / model), `[routing.cross_provider]` | `query_model = "mock-model"`, no providers |
| `retry` | `max_retries`, `initial_backoff_ms`, `jitter_ratio` | 3 / 500ms / 0.25 |
| `compression` | `enabled`, `trigger_ratio`, `keep_recent_turns`, `summarizer_model`, `max_summary_chars` | off / 0.75 / 4 / None / 2048 |
| `session` | `default_key`, `history_lines` | `"default"` / 10 |
| `tools` | `builtin_enabled`, `fs_root` | true / sandbox = harness.root |
| `harness` | `root`, `skills_roots`, `include_environment` | `"."` / `["skills"]` / true |

- **Secrets never hit disk**: `api_key` is always written as a `"$ENV_NAME"` placeholder, resolved from the environment at runtime; an unset variable causes that provider to be skipped.
- **Degradation semantics**:
  - Template left in placeholder state (`your-*` / `example.com`) → whole stack falls back to Mock.
  - A missing `$ENV` key → local degradation; routing candidates that reference it are dropped, and if `query_model` becomes dangling the whole stack falls back to Mock.
  - A single layer failing to parse (except ignored unknown fields) → that layer keeps its defaults and prints a warning.
- **Config selection**: an explicit `--config` wins; when omitted, `ribo.toml` is loaded only if present, otherwise fully-defaulted Mock mode is used.

A complete, copyable template is [`ribo_example.toml`](ribo_example.toml): copy it to `ribo.toml` and fill in real endpoints and keys.

```toml
# ribo.toml excerpt
query_model = "your-model-name"

[[providers]]
provider_id  = "chat-primary"
adapter      = "openai-chat"
base_url     = "https://your-endpoint.example.com"
api_key      = "$OPENAI_API_KEY"
model        = "your-model-name"

[routing.cross_provider]
"chat-primary" = ["anthropic-backup"]
```

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
| `read` | Read a file's contents. | limited to `fs_root` |
| `write` | Write a file. | limited to `fs_root` |
| `edit` | Edit a file in place. | limited to `fs_root` |
| `ls` | List a directory. | limited to `fs_root` |
| `glob` | Glob-match paths. | limited to `fs_root` |
| `grep` | Search file contents. | limited to `fs_root` |

The filesystem tools (read / write / edit / ls / glob / grep) share a single sandbox root `fs_root`; all paths are resolved inside the sandbox and out-of-bounds access is rejected.

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

- Scanned from `harness.skills_roots` (defaults to `["skills"]`, resolved relative to `root`, multiple roots allowed and loaded in order).
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
