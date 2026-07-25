# codex-gateway

A Rust gateway that bridges a local `codex app-server` to OpenAI-compatible Chat Completions and Responses APIs.

It translates HTTP requests from either API into Codex app-server JSON-RPC v2 messages and maps Codex events back to regular or streaming OpenAI-compatible responses.

```text
OpenAI-compatible client
   │  POST /v1/chat/completions (SSE)
   │  POST /v1/responses (SSE)
   ▼
codex-gateway
   │  JSON-RPC over stdio
   ▼
codex app-server
```

## Features

- `GET /v1/models`
- `POST /v1/chat/completions`
  - Regular text responses
  - Streaming
  - Forced function calls
- `POST /v1/responses`
  - Text input
  - Streaming
  - JSON Schema through `text.format`
- Bearer authentication
- Concurrent requests
- Codex turn cancellation on client disconnect or timeout

Both API styles are first-class gateway interfaces and use the same Codex app-server backend.

## Requirements

- Stable Rust
- Codex CLI 0.145.0 or later
- A working Codex login or API configuration
- An OpenAI-compatible client

Verify that the app-server command is available:

```bash
codex app-server --help
```

## Build

With Nix:

```bash
nix develop
cargo build
```

Build the Nix package directly:

```bash
nix build
./result/bin/codex-gateway --help
```

The Flake uses `nixpkgs-unstable`, `flake-parts`, `rust-overlay`, and `treefmt-nix`. The development shell and package build share the toolchain declared in `rust-toolchain.toml`.

`nix fmt` runs treefmt with nixfmt, rustfmt, and taplo. The development shell also configures sccache as `RUSTC_WRAPPER` to cache Rust recompilation.

Without Nix:

```bash
cargo build --release
```

Install directly from this repository:

```bash
cargo install --path .
```

## Run

Start the gateway with the workspace that Codex should be allowed to access:

```bash
export CODEX_BRIDGE_API_KEY='local-secret'

codex-gateway \
  --cwd /absolute/path/to/project \
  --listen 127.0.0.1:8787
```

When `--cwd` is omitted, the gateway uses its current working directory. It deliberately does not accept a working directory from request bodies, preventing remote clients from selecting arbitrary local paths.

Run a separate gateway on a different port for each workspace when serving multiple workspaces concurrently.

### Configuration

| Option / environment variable | Default | Description |
| --- | --- | --- |
| `--listen` / `CODEX_BRIDGE_LISTEN` | `127.0.0.1:8787` | HTTP listen address |
| `--api-key` / `CODEX_BRIDGE_API_KEY` | Required | Bearer token for the local HTTP API |
| `--cwd` / `CODEX_BRIDGE_CWD` | Startup directory | Codex working directory |
| `--codex-bin` / `CODEX_BRIDGE_CODEX_BIN` | `codex` | Path to the Codex CLI |
| `--codex-model` / `CODEX_BRIDGE_CODEX_MODEL` | Codex configuration | Optional Codex model override |
| `--model` / `CODEX_BRIDGE_MODEL` | `codex` | Model ID advertised by `/v1/models` |
| `--sandbox` / `CODEX_BRIDGE_SANDBOX` | `workspace-write` | `read-only`, `workspace-write`, or `danger-full-access` |
| `--timeout-secs` / `CODEX_BRIDGE_TIMEOUT_SECS` | `3600` | Timeout for one Codex turn |

`--no-auth` is accepted only when listening on a loopback address.

## Client configuration

Configure an OpenAI-compatible client with:

| Setting | Value |
| --- | --- |
| Base URL | `http://127.0.0.1:8787/v1` |
| API key | The value of `CODEX_BRIDGE_API_KEY` |
| Model | `codex` |

For clients that use the standard OpenAI environment variables:

```bash
export CODEX_BRIDGE_API_KEY='local-secret'
export OPENAI_API_KEY="$CODEX_BRIDGE_API_KEY"
export OPENAI_BASE_URL='http://127.0.0.1:8787/v1'
```

## API examples

Chat Completions API:

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_BRIDGE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "codex",
    "messages": [{"role": "user", "content": "Reply exactly pong."}]
  }'
```

Responses API:

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $CODEX_BRIDGE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"codex","input":"Reply exactly pong."}'
```

Streaming:

```bash
curl -N http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $CODEX_BRIDGE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"codex","input":"Explain this repository.","stream":true}'
```

## Permissions and security

- The HTTP API key is a local gateway token, not an OpenAI API key.
- `codex app-server` reads Codex authentication from the regular Codex configuration.
- The app-server approval policy is set to `never` for non-interactive operation.
- The default sandbox is `workspace-write`; network access and operations outside the sandbox are rejected.
- `danger-full-access` is enabled only when explicitly requested.
- Tool definitions received through Chat Completions requests are not exposed again to Codex. Codex works through its own app-server tools and returns the final result to the client.

## Limitations

- This is primarily a text compatibility layer. Image and audio input, hosted tools, and `previous_response_id` are not supported.
- Usage values are reported as zero because exact OpenAI token usage is not currently copied from Codex app-server events.
- Interactive approvals and questions are not relayed over the HTTP request. They are denied automatically or answered with an empty response.
- The Codex app-server protocol is experimental. Re-run the test suite after upgrading the Codex CLI.

## Development

The root package provides the `codex-gateway` binary, while library crates are
split by responsibility:

```text
src/
└── main.rs       # codex-gateway binary and dependency wiring
crates/
├── api/          # OpenAI-compatible HTTP routes and streaming
├── app-server/   # Codex app-server JSON-RPC transport
├── config/       # CLI and environment configuration
└── translate/    # Request-to-prompt translation
```

```bash
nix develop
nix fmt
treefmt --ci
cargo machete
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
sccache --show-stats
nix flake check
```

The implementation follows the app-server JSON Schema generated by:

```bash
codex app-server generate-json-schema --experimental
```
