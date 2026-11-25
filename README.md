# mcp-gw

MCP-шлюз на Rust: подключается к вашим MCP-серверам и отдаёт модели **прогрессивный поиск
инструментов** вместо того, чтобы грузить все определения тулов в контекст сразу.

Реализованы три паттерна из [MCP Client Best Practices](https://modelcontextprotocol.io/docs/2026-07-28/develop/clients/client-best-practices):

- **Progressive discovery** — каталог → инспекция → вызов (`search_tools` / `get_tool_details` / `call_tool`).
- **Dynamic server management** — серверы подключаются и отключаются по требованию
  (`search_servers` / `enable_server` / `disable_server`).
- **Programmatic tool calling (code mode)** — модель пишет JS в песочнице и склеивает
  несколько вызовов без прогона промежуточных результатов через контекст (`execute_code`).

## Как это работает

```
LLM / MCP client
      │  (stdio, JSON-RPC)
      ▼
  ┌─────────┐   search_servers / enable_server / disable_server
  │  mcp-gw │   search_tools / get_tool_details / call_tool
  │ (server)│   execute_code  ── QuickJS sandbox ── callTool()
  └────┬────┘
       │  (MCP client, stdio child process или Streamable HTTP)
       ▼
  ваши MCP-серверы (filesystem, github, ...)
```

Шлюз сам является MCP-сервером. Наружу он всегда отдаёт **фиксированный набор из 7
мета-тулов**, поэтому массив `tools` не меняется между ходами — это дружелюбно к prompt
caching. Определения upstream-тулов попадают в контекст только когда модель их запросит.

## Требования

- Rust 1.88+ (edition 2024, async-замыкания).
- Для upstream-серверов через `command` — соответствующий рантайм (например, `node`/`npx`).

## Сборка

```bash
cargo build --release
# бинарь: target/release/mcp-gw
# демо-сервер для проверки: target/release/demo_upstream
```

## Запуск

Шлюз читает конфиг из `MCP_GW_CONFIG`, по умолчанию — `./mcp.json`.

```bash
MCP_GW_CONFIG=/path/to/mcp.json ./target/release/mcp-gw
```

Общение идёт по stdio (JSON-RPC), поэтому запускать вручную в терминале не нужно —
его запускает ваш MCP-клиент.

### Подключение к MCP-клиенту

Добавьте шлюз как обычный stdio-сервер в конфиг клиента (Claude Desktop, Cursor,
MCP Inspector и т.п.):

```json
{
  "mcpServers": {
    "mcp-gw": {
      "command": "/abs/path/to/mcp-gw/target/release/mcp-gw",
      "env": {
        "MCP_GW_CONFIG": "/abs/path/to/mcp-gw/mcp.json"
      }
    }
  }
}
```

`mcp.json` — отдельный файл со списком **ваших** upstream-серверов (см. ниже).

## Конфиг `mcp.json`

Формат совместим с Claude Desktop, плюс два поля: `description` и `autoConnect`.

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/projects"],
      "env": { "SOME_TOKEN": "value" },
      "description": "Локальный доступ к файлам в ~/projects",
      "autoConnect": true
    },
    "remote": {
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer literal-token" },
      "description": "Удалённый MCP-сервер по Streamable HTTP",
      "autoConnect": false
    }
  }
}
```

| Поле | Транспорт | Описание |
|------|-----------|----------|
| `command` | stdio | Команда запуска дочернего процесса. |
| `args` | stdio | Аргументы команды. |
| `env` | stdio | Переменные окружения дочернего процесса. |
| `url` | HTTP | URL Streamable HTTP эндпоинта. |
| `headers` | HTTP | HTTP-заголовки (например, `Authorization`). |
| `description` | оба | Текст для `search_servers`. |
| `autoConnect` | оба | Подключаться на старте. По умолчанию `true`. |

Ровно один транспорт на сервер: либо `command`, либо `url`. Некорректные записи
пропускаются с предупреждением в stderr.

> Подстановки переменных окружения в `mcp.json` нет — значения записываются буквально.
> Держите файл с секретами вне системы контроля версий.

## Инструменты шлюза

### Dynamic server management

| Тул | Параметры | Что делает |
|-----|-----------|------------|
| `search_servers` | `query?`, `limit?` (по умолч. 20) | Список/поиск настроенных серверов: `name`, `description`, `connected`, `tool_count`. |
| `enable_server` | `name` | Подключает сервер и загружает его тулы. Идемпотентно. |
| `disable_server` | `name` | Отключает сервер и убирает его тулы из каталога. |

### Progressive discovery

| Тул | Параметры | Что делает |
|-----|-----------|------------|
| `search_tools` | `query`, `limit?` (10), `detail?` | Поиск по name/title/description/server. `detail`: `name` \| `summary` \| `full`. |
| `get_tool_details` | `name` | Полное определение: описание, `input_schema`, `output_schema`. |
| `call_tool` | `name`, `arguments?` | Вызов тула на его upstream-сервере; результат проксируется как есть. |

### Code mode

| Тул | Параметры | Что делает |
|-----|-----------|------------|
| `execute_code` | `code`, `timeout_ms?` (10000, макс. 60000) | Выполняет JS в песочнице. Доступны `await callTool(name, args)` и `console.*`. Возвращает `{ logs, result }` или `{ logs, error }`. |

Пример кода для `execute_code`:

```js
const a = await callTool("filesystem_read_file", { path: "/etc/hosts" });
const b = await callTool("web_fetch", { url: "https://example.com" });
console.log("прочитано", a.content[0].text.length, "и", b.content[0].text.length, "символов");
return { first: a.content[0].text.slice(0, 80) };
```

- `callTool` возвращает JSON-представление `CallToolResult` (`content`, `structuredContent`, `isError`, ...).
- Если upstream вернул `isError: true`, `callTool` **бросает исключение** — можно ловить через `try/catch`.
- Ошибки и таймауты скрипта возвращаются как тул-результат с `isError: true`, чтобы модель могла исправиться.

#### Безопасность песочницы

`execute_code` исполняется на встроенном **QuickJS** (`rquickjs`), а не через системный Node/Deno:

- нет доступа к файловой системе, сети, процессам и таймерам;
- наружу доступны только два моста — `callTool` и `console`;
- лимиты: память 64 MB, стек 512 KB, таймаут (по умолчанию 10 с) через interrupt handler,
  включая бесконечные синхронные циклы.

Это песочница уровня языка, а не OS-контейнер. Не запускайте непроверенный код в
доверенном окружении без дополнительной изоляции процесса.

## Пример: демо-сервер

В репозитории есть `demo_upstream` с тулами `echo` и `ping`.

```bash
cargo build
cat > /tmp/demo-mcp.json <<EOF
{
  "mcpServers": {
    "demo": {
      "command": "$(pwd)/target/debug/demo_upstream",
      "description": "Echo/ping демо",
      "autoConnect": false
    }
  }
}
EOF
MCP_GW_CONFIG=/tmp/demo-mcp.json ./target/debug/mcp-gw
```

Сценарий работы модели:

1. `search_servers` → видит `demo` (connected: false).
2. `enable_server { "name": "demo" }` → загружено 2 тула.
3. `search_tools { "query": "echo", "detail": "full" }` → находит `echo` со схемой.
4. `call_tool { "name": "echo", "arguments": { "text": "hi" } }` → `"hi"`.
5. Либо `execute_code`, чтобы вызвать несколько тулов за раз.

### Быстрая проверка через stdio

```bash
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
'{"jsonrpc":"2.0","method":"notifications/initialized"}' \
'{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
| MCP_GW_CONFIG=/tmp/demo-mcp.json ./target/debug/mcp-gw
```

## Поведение и детали

- **Коллизии имён.** Если два сервера отдают тул с одинаковым именем, второй получает
  префикс `server__tool` (например, `demo__echo`). Имена стабильны в пределах сессии.
- **Кэш определений.** Каталог хранится в памяти шлюза; повторная выдача определения не
  делает лишний `tools/list`.
- **`list_changed`.** На `notifications/tools/list_changed` от upstream шлюз
  перечитывает список и переиндексирует каталог (`src/upstream.rs`).
- **Ошибки вызова.** Неизвестный тул/сервер → JSON-RPC ошибка (`-32002` / `internal`);
  ошибка upstream-тула (`isError`) в `call_tool` проксируется как результат.

## Ограничения

- Поддерживаются только **tools** (resources/prompts не проксируются).
- Нет OAuth/авторизации для HTTP-серверов (только статические заголовки).
- Code mode — только JavaScript.
- `mcp.json` не поддерживает подстановку env-переменных.

## Разработка

```bash
cargo test          # юнит-тесты (конфиг, каталог, неймспейсинг)
cargo clippy --all-targets
cargo fmt
```

Структура:

| Файл | Назначение |
|------|-----------|
| `src/main.rs` | MCP-сервер шлюза и мета-тулы. |
| `src/config.rs` | Парсинг `mcp.json`. |
| `src/registry.rs` | Каталог тулов, реестр серверов, соединения. |
| `src/upstream.rs` | MCP-клиент к upstream, `enable`/`disable`, `list_changed`. |
| `src/code_mode.rs` | Песочница QuickJS и брокер `callTool`. |
| `src/bin/demo_upstream.rs` | Демо upstream-сервер для тестов. |
