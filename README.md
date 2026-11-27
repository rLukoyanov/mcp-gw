# tokenmaxing

A proxy for MCP (Model Context Protocol) that reduces token consumption by hiding
upstream tools behind a small set of discovery tools.

Instead of exposing every tool of every connected MCP server, `tokenmaxing`
advertises only four tools. The model searches for what it needs, reads the
schema, and then calls the upstream tool — all through the proxy.

## Features

- Speaks MCP to the client over stdio.
- Connects to any number of upstream MCP servers over stdio (`command`) or
  HTTP (`url`).
- Exposes four meta-tools instead of the full upstream tool catalog:
  - `search_servers` — find an upstream server by capability.
  - `search_tools` — find tools by name or description.
  - `get_tool_details` — read a tool's full input schema.
  - `call_tool` — invoke an upstream tool and relay the result unchanged.
- Connects servers at startup or lazily, per server (`autoConnect`).
- A broken upstream does not take the proxy down: it is reported as
  disconnected with its error.

## Quick start

Build the binary:

```bash
git clone https://github.com/rLukoyanov/tokenmaxing.git
cd tokenmaxing
go build -o tokenmaxing .
```

Write a proxy configuration listing upstream MCP servers:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "LOG_LEVEL": "warn" },
      "description": "Local filesystem access"
    },
    "github": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ..." },
      "description": "GitHub repositories and issues",
      "autoConnect": false
    }
  }
}
```

Validate it:

```bash
./tokenmaxing config validate --config ./config.json
# config.json: ok, 2 MCP server(s) configured
```

## Connecting a client

The client launches `tokenmaxing serve` as a normal stdio MCP server. Use
absolute paths for both the binary and the configuration file.

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`
on macOS):

```json
{
  "mcpServers": {
    "tokenmaxing": {
      "command": "/path/to/tokenmaxing",
      "args": ["serve", "--config", "/path/to/config.json"]
    }
  }
}
```

Claude Code:

```bash
claude mcp add tokenmaxing -- /path/to/tokenmaxing serve --config /path/to/config.json
```

Restart the client and confirm the server is listed (`claude mcp list`).

## Configuration

The configuration file is JSON and compatible with Claude Desktop's
`mcpServers` format. Unknown fields are ignored, so existing Claude
configurations can be reused.

```json
{
  "mcpServers": {
    "<name>": {
      "command": "npx",
      "args": ["-y", "some-mcp-server"],
      "env": { "KEY": "value" },
      "description": "What this server provides",
      "autoConnect": true
    },
    "<remote-name>": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ..." },
      "description": "What this server provides",
      "autoConnect": false
    }
  }
}
```

| Field | Applies to | Description |
|---|---|---|
| `command` | stdio | Executable that starts a local MCP server. |
| `args` | stdio | Arguments for `command`. |
| `env` | stdio | Extra environment variables for `command`. |
| `url` | HTTP | Endpoint of a remote MCP server (`http` or `https`). |
| `headers` | HTTP | Headers added to every request to the remote server. |
| `description` | both | Human-readable description used by `search_servers`. |
| `autoConnect` | both | Connect at startup. Defaults to `true`. |

Exactly one of `command` or `url` must be set per server. Server names must be
non-empty and unique (they are JSON object keys).

### Path resolution

The configuration file is looked up in this order:

1. `--config <path>`
2. `TOKENMAXING_CONFIG` environment variable
3. `<user config dir>/tokenmaxing/config.json`
   (on macOS: `~/Library/Application Support/tokenmaxing/config.json`)

### autoConnect

- `true` or omitted — the server is connected during startup and its tools are
  indexed immediately. `search_tools` finds its tools without naming the server.
- `false` — the server is not connected at startup and its tools are not
  requested. It still appears in `search_servers` with `connected: false`.
  It connects on demand when the model names it in `search_tools(server=...)`,
  `get_tool_details(server=...)`, or `call_tool`.

## Tools

### `search_servers`

```json
{ "query": "filesystem", "limit": 5 }
```

Returns compact server summaries: `name`, `description`, `transport`,
`autoConnect`, `connected`, `toolCount`, and `error` if the connection failed.
An empty query lists all servers, up to `limit`.

### `search_tools`

```json
{ "query": "read a file", "server": "filesystem", "limit": 5 }
```

Returns tools across connected servers: `server`, `name`, `description`.
`server` narrows the search and connects an on-demand server first. Without
`server`, only already connected servers are searched.

### `get_tool_details`

```json
{ "tool": "read_file", "server": "filesystem" }
```

Returns the full `inputSchema`. An exact, case-insensitive name match is
preferred; otherwise tools whose name contains the query are returned.

### `call_tool`

```json
{
  "server": "filesystem",
  "tool": "read_file",
  "arguments": { "path": "/tmp/notes.txt" }
}
```

Connects the server if needed and returns the upstream result unchanged,
including `content`, `structuredContent`, and `isError`.

### Typical flow

```
search_servers   {"query": "filesystem"}
  → filesystem: connected=true, toolCount=2

search_tools     {"query": "read", "server": "filesystem"}
  → filesystem/read_file

get_tool_details {"tool": "read_file", "server": "filesystem"}
  → full inputSchema

call_tool        {"server": "filesystem", "tool": "read_file",
                  "arguments": {"path": "/tmp/notes.txt"}}
  → upstream result
```

## Logging and troubleshooting

- Logs go to **stderr**; stdout is reserved for the MCP protocol. They appear in
  the client's MCP server logs.
- Add `-v` for debug-level logging:
  ```bash
  ./tokenmaxing serve --config ./config.json -v
  ```
- On startup the proxy logs each connection attempt:
  ```
  level=INFO msg="connected upstream server" server=filesystem tools=2
  level=WARN msg="upstream server unavailable" server=dead error="..."
  ```
- `no upstream MCP servers could be connected` — every configured server failed;
  check `config validate` and each server's `command`/`url`.
- A server that fails to connect stays visible in `search_servers` with
  `connected: false` and the error text.

## Limitations

- Tool lists are captured when a server connects; changes are not refreshed
  automatically. Restart the proxy after changing an upstream's tools.
- `call_tool` relays results unchanged — result compression is not implemented.
- There is no runtime enable/disable tool; `autoConnect` controls whether a
  server starts connected.

## Development

```bash
go test ./...          # unit and integration tests (spawns real stdio servers)
go test -race ./...
go vet ./...
```

Layout:

```
main.go                     CLI entry point
cmd/                        Cobra commands: serve, config, version
internal/config/            configuration parsing, validation, path resolution
internal/upstream/          MCP client connections (stdio, HTTP)
internal/gateway/           tool index, discovery tools, lazy connections
```
