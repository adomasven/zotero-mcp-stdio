# zotero-mcp-stdio

A small stdio [MCP](https://modelcontextprotocol.io) server that AI apps (Claude Code, Claude Desktop, Codex, the ChatGPT desktop app, Cursor, VS Code) launch to reach a running Zotero. It ships inside Zotero; Zotero copies it to a stable per-user location on launch and registers it with AI apps when the user asks.

It holds no Zotero logic. It:

- serves the tool list from `tools.json` next to the executable, so the list is the same whether or not Zotero is running;
- forwards each tool call as MCP JSON-RPC to `POST http://127.0.0.1:<port>/mcp` on the running Zotero;
- starts Zotero when it isn't running, using the command Zotero recorded, and waits for it;
- turns every failure (Zotero not installed, not running, too old, waiting for the user to approve a request) into a tool result the model can read and relay.

It speaks both protocol eras on stdio (2026-07-28 and the `initialize`-based 2025-11-25 and earlier) via [rmcp](https://crates.io/crates/rmcp), and always uses 2026-07-28 towards Zotero.

## Usage

```
zotero-mcp-stdio [--client <ID>] [--profile <DIR>] [--tools <FILE>] [--call-timeout <SECONDS>]
```

- `--client`: identifies the AI app (`claude-desktop`, `codex`, …). Sent to Zotero as `Zotero-MCP-Client`, for per-app permissions.
- `--profile`: Zotero profile directory. Defaults to the default profile in `profiles.ini`, chosen as in `Zotero.Profile.getDefaultInProfilesDir()`. `profiles.ini` is looked up where Gecko keeps it: `~/Library/Application Support/Zotero` (macOS), `%APPDATA%\Zotero\Zotero` (Windows), and on Linux `~/.zotero/zotero` if it exists, otherwise `$XDG_CONFIG_HOME/zotero/zotero`.
- `--tools`: tool definitions. Defaults to `tools.json` next to the executable. If it can't be read, the server exits with an error, so the AI app shows it as failed rather than as having no tools.
- `--call-timeout`: how long to wait for one call, in seconds (default 50). Past this, the model is told Zotero may be waiting for the user's approval and to call again.

Logs go to stderr.

## Files Zotero provides

`tools.json`, next to the executable:

```json
{ "tools": [ { "name": "…", "description": "…", "inputSchema": { "type": "object" } } ] }
```

`zotero-mcp.json`, in the profile directory, rewritten on every Zotero launch. `version` is the file format; a sidecar with a format this build doesn't know is reported to the model as an error instead of being misread. Adding fields doesn't need a new format.

```json
{
  "version": 1,
  "zoteroVersion": "10.1",
  "port": 23119,
  "launch": { "command": "/usr/bin/open", "args": ["-a", "/Applications/Zotero.app"] }
}
```

## Requests to Zotero

One HTTP/1.1 `POST /mcp` per tool call, with `Host: 127.0.0.1:<port>`, no `Origin` header (Zotero's server drops browser requests), `Content-Type: application/json`, `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, and `Zotero-MCP-Client`. The body is a `tools/call` request with per-request `_meta`. Zotero answers with a JSON-RPC response; a 404 means the endpoint is missing or turned off.

## Building

```
cargo test
cargo clippy --all-targets
cargo build --release
```

`rust-toolchain.toml` pins the Rust version Zotero uses. CI runs formatting, clippy and tests on Linux. It builds every platform Zotero ships on native GitHub runners and publishes `ci/zotero-mcp-stdio/<commit>.zip` to the `zotero-download` bucket, laid out as:

```
mac/zotero-mcp-stdio            universal (arm64 + x86_64)
win-x64/zotero-mcp-stdio.exe
win-arm64/zotero-mcp-stdio.exe
win32/zotero-mcp-stdio.exe
linux-x64/zotero-mcp-stdio      static (musl)
linux-arm64/zotero-mcp-stdio    static (musl)
```

Binaries are unsigned; Zotero's `app/build.sh` signs them when packaging.
