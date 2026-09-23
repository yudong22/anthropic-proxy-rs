# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`anthropic-proxy-rs` is a high-performance Rust proxy that translates the **Anthropic Messages API** (`/v1/messages`) into **OpenAI-compatible** (`/v1/chat/completions`) and **OpenAI Responses API** (`/v1/responses`) formats, distributed as a **Tauri 2 desktop app** (`src-tauri`). It lets Anthropic clients (Claude Code, Claude Desktop, Codex) talk to upstreams like OpenRouter, OpenAI, Ollama, and WorkBuddy.

Two front-ends share one proxy core:
- **Proxy core library** (`anthropic_proxy`, `src/`) — GUI-agnostic, no Tauri dependency.
- **Desktop GUI** (`anthropic-proxy-gui`, `src-tauri/`) — a Cargo workspace member depending on the library, plus `ui/` (plain HTML/CSS/JS, no build step).

There is no headless binary crate; the same library is reused by both `src-tauri/src/main.rs` and any future front-end.

## Common Commands

This repo uses [Task](https://taskfile.dev) (`Taskfile.yaml`). Plain cargo/npm equivalents are shown where useful.

### Development (desktop GUI)
```bash
task setup      # npm ci — installs the pinned Tauri CLI (Node dev-dep)
task dev        # npm run dev — Tauri 2 GUI in development mode
task build      # npm run build — build the .app bundle
task build-dmg  # npm run build:dmg — build the .dmg installer
task install    # build + install to /Applications (restarts if running)
```

### Quality gate (runs in CI)
```bash
task fmt         # cargo fmt
task fmt-check   # cargo fmt -- --check   (CI runs this)
task lint        # cargo clippy --all-targets -- -D warnings   (CI runs this)
task test        # cargo test             (CI runs this)
task check       # fmt-check + lint + test in sequence
```
CI (`.github/workflows/ci.yml`) requires `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` to all pass. **Write code that passes all three before committing.**

### Single test / targeted runs
```bash
cargo test translate::responses::tests::<name>   # run one test by path
cargo test -p anthropic-proxy                     # library only (excludes GUI)
cargo test --lib                                 # lib unit tests (excludes src-tauri)
cargo test -- --nocapture --test-threads=1        # (task test-verbose) see logs
```

### Other utilities
```bash
task watch    # cargo watch auto-restart of the GUI
task audit    # cargo audit — security advisories
task doc      # cargo doc --open --no-deps
```

## Architecture: Layered Functional Translation

The core design is a strict **4-layer hierarchy** (`specs/architecture.md` is the authoritative spec). Routes converge so that all three API flavors, streaming and non-streaming, share one upstream-forwarding function.

```
LAYER 0  src/models/{anthropic,openai,responses}.rs   Wire-format data types only.
LAYER 1  src/translate/core.rs                        Atomic pure mappings (1 concept per fn).
LAYER 2a src/translate/pipeline.rs                    Config-aware batch translation + policy.
LAYER 2b src/translate/{stream,responses}.rs          Pure SSE stream state machines.
LAYER 3  src/proxy.rs, config.rs, service.rs,         I/O shell: HTTP, SSE framing, config,
         router.rs, settings.rs, stats.rs,            logging, lifecycle. No business logic.
         credits.rs, providers.rs, metrics.rs
```

**Request flow (`/v1/messages` streaming):** `proxy::proxy_handler` → check service running (else 503) → `translation_policy(&config)` → `pipeline::translate_request(req, &policy)` → `forward_request(...)` → `create_flavor_sse_stream(upstream, flavor)` which calls `translate::stream::translate_chunk` per frame → `record_request` + token stats.

### Key invariants (enforced by the architecture — do not violate)
1. **Layers 0/1/2a/2b contain NO I/O, async, or logging** — they are pure functions, easy to unit-test (115 tests in `translate/`).
2. **Layer 3 contains NO business logic** — only wiring. Handlers call the pure `translate/*` functions.
3. **`translate/` never imports `proxy.rs`, `config.rs`, `router.rs`, or `settings.rs`.**
4. **`proxy.rs` never constructs Anthropic `StreamEvent`s directly** — only via `translate::stream.rs` / `translate::responses.rs`.
5. **All routes are registered exactly once**, in `src/router.rs::build_app_router`. Both front-ends (headless and Tauri) build their listener from this one function, so a new route appears in both automatically. Neither binary should hand-roll a router.
6. **The desktop app must build `Config` via `Config::from_settings`** (`config.rs:91`) — never assemble it inline.
7. State in `translate/` is passed explicitly (no globals/Arc).

### Important structural details
- **`ApiFlavor` enum** (`proxy.rs:25`) — `Anthropic | Responses | Chat`. `forward_request`, retry, SSE framing, and stats all branch on it; adding a protocol means adding a `models/` type + a `translate/` function + one `ApiFlavor` arm, reusing the rest.
- **`TranslationPolicy`** (`translate/pipeline.rs:9`) — the config slice the pure layer sees: `{reasoning_model, completion_model, model_map, ignore_terms}`. Built by `translation_policy(config)` (`proxy.rs:899`).
- **`forward_request`** (`proxy.rs:441`) — single multi-upstream failover function. `UPSTREAM_BASE_URL` is `;`-separated; retries to the next upstream **only** on 429/5xx, fast-fails otherwise. Model mapping (`ANTHROPIC_PROXY_MODEL_MAP`) applies after reasoning/completion selection.
- **`ModelsFlavor`** (`config.rs`) — `OpenAI` (default) vs `WorkBuddyConfig`. When WorkBuddy, `sanitize_fingerprints` neutralizes content-filter fingerprints across outbound message fields (`pipeline.rs`, port of Go sanitize table), and `models_config_url` drives model discovery (`providers.rs`).
- **Stream state machines** (`translate/stream.rs` `StreamState`/`BlockState`) enforce the invariant that every `ContentBlockStart` is paired with exactly one `ContentBlockStop`.

## Configuration & Persistence

- Priority: **env / `.env` → overrides `~/.proxy-rs/gui-settings.json`** app settings. `.env` search order: `./.env` → `~/.proxy-rs/.env` → `~/.anthropic-proxy.env` → `/etc/anthropic-proxy/.env` (first found wins).
- `UPSTREAM_BASE_URL` is required; `;` for failover. `UPSTREAM_API_KEY_PASSTHROUGH=true` and `UPSTREAM_API_KEY` are mutually exclusive (startup is refused if both set).
- `UPSTREAM_BASE_URL` accepts a service root, a versioned root, or a full endpoint — but rejects query params/fragments/partial paths.
- All state lives in `~/.proxy-rs/`: `gui-settings.json`, `.env`, `logs/proxy.log`, `stats.db` (SQLite daily stats). `gui-settings.json` is loaded by `Config::from_settings`.
- Env flags: `REASONING_MODEL`, `COMPLETION_MODEL`, `ANTHROPIC_PROXY_MODEL_MAP`, `ANTHROPIC_PROXY_SYSTEM_PROMPT_IGNORE_TERMS`, `DEBUG`/`VERBOSE`.

## HTTP Interface (registered in `router.rs`)
| Method | Path(s) | Purpose |
|--------|---------|---------|
| POST | `/v1/messages` | Anthropic Messages API |
| POST | `/v1/responses`, `/responses`, `/backend-api/codex/responses` | OpenAI Responses API |
| POST | `/v1/chat/completions`, `/chat/completions` | OpenAI Chat Completions passthrough |
| GET | `/v1/models`, `/models` | Model list (Anthropic format) |
| GET | `/v1/credits`, `/credits` | Gateway wallet balance |
| GET | `/health` | returns `OK` |
| GET | `/metrics` | Prometheus metrics |

## Where to Extend
- **New provider preset:** add a `ProviderPreset` in `src/providers.rs::builtin_presets()` (needs `chat_completions_url`; `models_config_url` if it has a vendor models catalog).
- **New route:** add `.route(...)` in `build_app_router` (`router.rs`).
- **New protocol:** add data model in `src/models/`, pure translation in `src/translate/`, then one `ApiFlavor` branch in `proxy.rs`.
- **New Tauri command:** write `#[tauri::command]` in `src-tauri/src/main.rs`, register in `invoke_handler`, call via `invoke(...)` in `ui/app.js`.

## Known Limitations
Not supported: `tool_choice` (fixed `auto`), `service_tier`, `metadata`, `context_management`, `container`, citations, `pause_turn`/`refusal` stop reasons, Batches/Files/Admin API.
