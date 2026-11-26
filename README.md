# tokenmaxing

MCP-шлюз на Rust: подключается к вашим MCP-серверам и отдаёт модели **прогрессивный
поиск инструментов** вместо того, чтобы грузить все определения тулов в контекст сразу.

- **Стартовый контекст — на 93% меньше**: на каталоге из 42 тулов 19 416 → 1 367 токенов.
- **Стабильный набор из 7 мета-тулов в discovery**: массив `tools` не меняется между
  ходами, что дружелюбно к prompt caching.
- **Гибридный режим**: маленький каталог отдаётся напрямую (`passthrough`), большой —
  через прогрессивный поиск; режим выбирается автоматически.
- **Сжатие крупных результатов** через Headroom: до −49% на JSON и логах.
- Два транспорта до upstream (**stdio** и **Streamable HTTP**), песочница **QuickJS**
  для code mode, **Prometheus-метрики**, наборы агентов для Claude Code и opencode.

Замеры, методика и честные оговорки — в **[RESULTS.md](RESULTS.md)**.

## Зачем это нужно

Прямое подключение всех MCP-серверов кладёт в контекст каждое определение каждого тула.
42 тула — это ~19 400 токенов ещё до первого вопроса, и цена растёт с каждым новым
сервером. Шлюз реализует три паттерна из
[MCP Client Best Practices](https://modelcontextprotocol.io/docs/2026-07-28/develop/clients/client-best-practices):

- **Progressive discovery** — каталог → инспекция → вызов
  (`search_tools` / `get_tool_details` / `call_tool`).
- **Dynamic server management** — серверы подключаются и отключаются по требованию
  (`search_servers` / `enable_server` / `disable_server`).
- **Programmatic tool calling (code mode)** — модель пишет JS в песочнице и склеивает
  несколько вызовов без прогона промежуточных результатов через контекст (`execute_code`).

В режиме **discovery** наружу отдаются **7 мета-тулов**, и определения upstream-тулов
попадают в контекст только по запросу. В режиме **passthrough** (для маленьких каталогов)
тулы отдаются напрямую, как без шлюза. Режим выбирается автоматически или явно через
`TOKENMAXING_MODE` — см. [Режимы работы](#режимы-работы).

## Быстрый старт

### 1. Соберите

Нужен **Rust 1.88+** (edition 2024, let-chains). Node.js/`npx` понадобится только для
upstream-серверов, которые запускаются через Node.

```bash
git clone https://github.com/rLukoyanov/tokenmaxing.git
cd tokenmaxing
cargo build --release
```

Соберутся три бинаря:

| Бинарь | Назначение |
|--------|-----------|
| `target/release/tokenmaxing` | сам шлюз (MCP-сервер по stdio) |
| `target/release/demo_upstream` | демо upstream с тулами `echo` и `ping` |
| `target/release/demo_headroom` | заглушка Headroom для проверки авто-сжатия |

### 2. Проверьте за минуту (без сети)

Создайте временный конфиг с демо-сервером. Замените `/ABS/PATH/tokenmaxing` на абсолютный
путь к репозиторию:

```bash
cat > /tmp/tokenmaxing-demo.json <<'EOF'
{
  "mcpServers": {
    "demo": {
      "command": "/ABS/PATH/tokenmaxing/target/release/demo_upstream",
      "description": "Echo/ping demo",
      "autoConnect": false
    }
  }
}
EOF
```

Теперь отправьте шлюзу `initialize` и `tools/list` по stdio:

```bash
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
'{"jsonrpc":"2.0","method":"notifications/initialized"}' \
'{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
| TOKENMAXING_METRICS_ADDR=off TOKENMAXING_CONFIG=/tmp/tokenmaxing-demo.json ./target/release/tokenmaxing
```

В ответе на `tools/list` будут ровно семь имён:

```
call_tool, disable_server, enable_server, execute_code,
get_tool_details, search_servers, search_tools
```

Строка `gateway ready with 0 tools` в stderr — норма: демо-сервер ленивый
(`autoConnect: false`) и подключается только по запросу модели.

> Здесь выбран discovery, потому что демо-сервер ленивый. Если все серверы
> `autoConnect: true`, подключились, и тулов не больше 15, `auto` выберет
> passthrough — тогда `tools/list` вернёт `echo` и `ping` напрямую.

Дальше модель (или MCP-клиент) проходит цикл:

1. `search_servers` → видит `demo`, `connected: false`.
2. `enable_server { "name": "demo" }` → подключено 2 тула.
3. `search_tools { "query": "echo", "detail": "full" }` → находит `echo` со схемой.
4. `call_tool { "name": "echo", "arguments": { "text": "hi" } }` → возвращает `"hi"`.
5. Для цепочки вызовов — `execute_code` (см. [ниже](#code-mode)).

### 3. Подключите к MCP-клиенту

Добавьте шлюз как обычный stdio-сервер в Claude Desktop, Cursor, MCP Inspector или
другой MCP-клиент:

```json
{
  "mcpServers": {
    "tokenmaxing": {
      "command": "/abs/path/to/tokenmaxing/target/release/tokenmaxing",
      "env": {
        "TOKENMAXING_CONFIG": "/abs/path/to/mcp.json"
      }
    }
  }
}
```

- `TOKENMAXING_CONFIG` указывает на **ваш** файл со списком upstream-серверов. Если не задать,
  шлюз ищет `./mcp.json` относительно своего рабочего каталога.
- Клиент сам запускает шлюз и общается с ним по stdio (JSON-RPC). Вручную в терминале
  его запускать не нужно.
- Первый запуск с `npx`-серверами может быть медленнее: `npx` скачивает пакеты.

### 4. Создайте свой `mcp.json`

В репозитории нет готового `mcp.json`: файл содержит ваши пути и секреты. Создайте его
рядом с конфигом клиента — формат совместим с Claude Desktop плюс поля `description`
и `autoConnect`:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/abs/path/to/workspace"],
      "description": "Read and search files in the workspace",
      "autoConnect": true
    },
    "memory": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-memory"],
      "description": "Knowledge graph memory across conversations",
      "autoConnect": true
    },
    "deepwiki": {
      "url": "https://mcp.deepwiki.com/mcp",
      "description": "Public DeepWiki MCP for GitHub repository docs (no auth)",
      "autoConnect": false
    }
  }
}
```

`filesystem` и `memory` подключатся на старте, `deepwiki` — по `enable_server`.
Полное описание формата — в разделе [Конфигурация](#конфигурация-mcpjson).

## Содержание

- [Как это работает](#как-это-работает)
- [Режимы работы](#режимы-работы)
- [Конфигурация `mcp.json`](#конфигурация-mcpjson)
- [Инструменты шлюза](#инструменты-шлюза)
- [Авто-сжатие через Headroom](#авто-сжатие-через-headroom)
- [Метрики](#метрики)
- [Экономия токенов](#экономия-токенов)
- [Переменные окружения](#переменные-окружения)
- [Поведение и детали](#поведение-и-детали)
- [Ограничения](#ограничения)
- [Диагностика](#диагностика)
- [Разработка](#разработка)

## Как это работает

```
LLM / MCP client
      │  (stdio, JSON-RPC)
      ▼
  ┌─────────────┐   search_servers / enable_server / disable_server
  │ tokenmaxing │   search_tools / get_tool_details / call_tool
  │  (server)   │   execute_code  ── QuickJS sandbox ── callTool()
  └──────┬──────┘
       │  (MCP client, stdio child process или Streamable HTTP)
       ▼
  ваши MCP-серверы (filesystem, github, ...)
```

Шлюз сам является MCP-сервером. В discovery-режиме модель видит только мета-тулы;
upstream-тулы появляются в каталоге после `enable_server` и остаются там, пока сервер
подключён. В passthrough-режиме каталог виден модели напрямую.

| Слой | Что происходит |
|------|----------------|
| Каталог | `search_servers` показывает настроенные серверы и их статус. |
| Discovery | `search_tools` ищет по имени, title, описанию и серверу; `detail` управляет объёмом. |
| Инспекция | `get_tool_details` отдаёт полные схемы одного или нескольких тулов. |
| Вызов | `call_tool` маршрутизирует вызов на upstream и проксирует результат. |
| Композиция | `execute_code` выполняет JS и может вызывать `callTool` многократно. |

### Режимы работы

По умолчанию `TOKENMAXING_MODE=auto`: шлюз сам выбирает режим после подключения серверов.

| Режим | Что видит модель | Когда выбирается |
|-------|------------------|------------------|
| `discovery` | 7 мета-тулов | Большой каталог, ленивые (`autoConnect: false`) серверы или ошибка подключения. |
| `passthrough` | Тулы подключённых upstream-серверов напрямую | `auto`: все настроенные серверы подключены и в каталоге не больше `TOKENMAXING_PASSTHROUGH_MAX_TOOLS` тулов. |

- **`auto`** (по умолчанию) — passthrough только для маленького полностью подключённого
  каталога; иначе discovery. Так discovery-накладные не платятся там, где не окупаются,
  а на больших каталогах сохраняется экономия контекста.
- **`discovery`** — всегда 7 мета-тулов: `search_tools`, `get_tool_details`, `call_tool`,
  `execute_code`, `search_servers`, `enable_server`, `disable_server`.
- **`passthrough`** — модель видит определения upstream-тулов напрямую, как без шлюза.
  Авто-сжатие Headroom продолжает работать. Мета-тулы и `execute_code` в этом режиме
  недоступны, а ленивые/отключённые серверы не попадают в список.

Порог задаётся `TOKENMAXING_PASSTHROUGH_MAX_TOOLS` (1–10 000, по умолчанию 15).
В passthrough шлюз пересылает клиенту `notifications/tools/list_changed`, когда upstream
меняет набор тулов; в discovery `tools/list` стабилен, а каталог обновляется внутренне.

```bash
# Принудительно discovery
TOKENMAXING_MODE=discovery ./target/release/tokenmaxing

# Auto-passthrough с более высоким порогом
TOKENMAXING_PASSTHROUGH_MAX_TOOLS=40 ./target/release/tokenmaxing
```

## Конфигурация `mcp.json`

Формат совместим с Claude Desktop, плюс два поля: `description` и `autoConnect`.
Опциональный блок `compression` включает авто-сжатие результатов.

```json
{
  "compression": {
    "server": "headroom",
    "minBytes": 512
  },
  "mcpServers": {
    "headroom": {
      "command": "headroom",
      "args": ["mcp", "serve"],
      "description": "Headroom compression/retrieval"
    },
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

### Поля сервера

| Поле | Транспорт | Описание |
|------|-----------|----------|
| `command` | stdio | Команда запуска дочернего процесса. |
| `args` | stdio | Аргументы команды. |
| `env` | stdio | Переменные окружения дочернего процесса. |
| `url` | HTTP | URL Streamable HTTP эндпоинта. |
| `headers` | HTTP | HTTP-заголовки (например, `Authorization`). |
| `description` | оба | Текст для `search_servers`. |
| `autoConnect` | оба | Подключаться на старте. По умолчанию `true`. Ленивый сервер оставляет auto-режим в discovery. |

Правила:

- Ровно один транспорт на сервер: либо `command`, либо `url`.
  Некорректные записи пропускаются с предупреждением в stderr.
- `autoConnect: false` оставляет сервер в конфиге, но не подключает его до
  `enable_server`. Это удобно для тяжёлых или редких серверов.
- Если файл не найден или не разбирается, шлюз пишет предупреждение и стартует
  с пустым каталогом — клиент при этом продолжает работать.

> Подстановки переменных окружения в `mcp.json` нет — значения записываются буквально.
> Держите файл с секретами вне системы контроля версий.

### Рекомендуемый стартовый набор

Пять серверов, которые удобно взять за основу (~45 инструментов):

| Сервер | Что даёт | Инструментов |
|--------|----------|--------------|
| `filesystem` | чтение/запись/поиск файлов в выбранном каталоге | 16 |
| `memory` | граф знаний (сущности и связи) | 9 |
| `sequential-thinking` | пошаговое рассуждение | 1 |
| `everything` | демо-сервер со множеством примеров | 16 |
| `deepwiki` | публичный DeepWiki MCP (docs по GitHub-репозиториям, `url`) | 3 |

Первые четыре запускаются через `npx`, поэтому первый старт может быть медленнее.
`deepwiki` — публичный HTTPS без токена; держите его ленивым (`autoConnect: false`).
`sequential-thinking` и `everything` добавляются в `mcpServers` так же, как `memory`,
с соответствующими пакетами `@modelcontextprotocol/server-*`.

Чтобы добавить сервер с секретом (GitHub, поиск и т.п.), допишите запись в `mcp.json`
с `env`/`headers` и при необходимости `autoConnect: false`.

### Агенты (10 + 10)

В репозитории лежат два набора ролей, все работают через MCP-шлюз:

- **Claude Code**: `.claude/agents/tokenmaxing-*.md` — 10 субагентов (researcher, reviewer,
  planner, debugger, tester, docs-writer, refactorer, security-auditor,
  performance-analyst, release-manager). Claude Code подхватывает их при старте;
  вызов — через делегирование субагентам.
- **opencode**: `.opencode/agent/tokenmaxing-*.md` — те же 10 ролей с `mode: subagent`.
  После добавления файлов перезапустите opencode; субагенты вызываются через
  инструмент `task` из primary-агента (напрямую `--agent tokenmaxing-*` нельзя — они subagent).

Промпты инструктируют сначала искать возможности (`search_servers`/`search_tools`),
читать схему (`get_tool_details`), вызывать (`call_tool`), а для цепочек использовать
`execute_code`; при сжатом результате — `headroom_retrieve`.

## Инструменты шлюза

Мета-тулы доступны в **discovery**-режиме. В **passthrough** их нет: модель вызывает
upstream-тулы напрямую, а шлюз только маршрутизирует вызов и при необходимости сжимает
результат.

### Dynamic server management

| Тул | Параметры | Что делает |
|-----|-----------|------------|
| `search_servers` | `query?`, `limit?` (по умолч. 20, макс. 100) | Список/поиск настроенных серверов: `name`, `description`, `connected`, `tool_count`. |
| `enable_server` | `name` | Подключает сервер и загружает его тулы. Идемпотентно. |
| `disable_server` | `name` | Отключает сервер и убирает его тулы из каталога. |

### Progressive discovery

| Тул | Параметры | Что делает |
|-----|-----------|------------|
| `search_tools` | `query`, `limit?` (10, макс. 50), `detail?` | Поиск по name/title/description/server. `detail`: `name` (по умолч.) \| `summary` (одна строка) \| `schema` (компактная схема) \| `full` (полная схема). |
| `get_tool_details` | `name?` или `names?` | Полное определение: описание, `input_schema`, `output_schema`. `name` возвращает объект, `names` (до 20) — `{ tools, missing }`. |
| `call_tool` | `name`, `arguments?` | Вызов тула на его upstream-сервере; результат проксируется как есть. |

Уровни `detail` позволяют платить за discovery постепенно:

- `name` (по умолчанию) — только имя и сервер; минимальная цена запроса;
- `summary` — плюс title и описание, схлопнутое в одну строку (до 200 символов);
- `schema` — плюс компактная `input_schema` без `$schema`, описаний, примеров и
  значений по умолчанию;
- `full` — полная `input_schema`, как у upstream.

`detail: "schema"` или `"full"` в `search_tools` — быстрый способ получить несколько
схем одним вызовом. `get_tool_details` с `names` — то же самое в полном виде для
конкретных тулов (до 20 за вызов). Для экономии контекста начинайте с `name` и
поднимайте детализацию только для реальных кандидатов.

### Code mode

Доступен только в discovery-режиме.

| Тул | Параметры | Что делает |
|-----|-----------|------------|
| `execute_code` | `code`, `timeout_ms?` (10 000; допустимо 100–60 000) | Выполняет JS в песочнице. Доступны `await callTool(name, args)` и `console.*`. Возвращает `{ logs, result }` или `{ logs, error }`. |

Пример кода для `execute_code`:

```js
const a = await callTool("filesystem_read_file", { path: "/etc/hosts" });
const b = await callTool("web_fetch", { url: "https://example.com" });
console.log("прочитано", a.content[0].text.length, "и", b.content[0].text.length, "символов");
return { first: a.content[0].text.slice(0, 80) };
```

- `callTool` возвращает JSON-представление `CallToolResult`
  (`content`, `structuredContent`, `isError`, ...).
- Если upstream вернул `isError: true`, `callTool` **бросает исключение** — можно
  ловить через `try/catch`.
- Ошибки и таймауты скрипта возвращаются как тул-результат с `isError: true`,
  чтобы модель могла исправиться.
- Промежуточные результаты `callTool` не попадают в контекст — возвращаются только
  `logs` и итоговое значение.

#### Безопасность песочницы

`execute_code` исполняется на встроенном **QuickJS** (`rquickjs`), а не через системный
Node/Deno:

- нет доступа к файловой системе, сети, процессам и таймерам;
- наружу доступны только два моста — `callTool` и `console`;
- лимиты: память 64 MB, стек 512 KB, таймаут (по умолчанию 10 с) через interrupt
  handler, включая бесконечные синхронные циклы.

Это песочница уровня языка, а не OS-контейнер. Не запускайте непроверенный код в
доверенном окружении без дополнительной изоляции процесса.

## Авто-сжатие через Headroom

Если указан блок `compression`, каждый результат `call_tool` крупнее `minBytes`
автоматически прогоняется через `headroom_compress` указанного upstream-сервера.
Модель получает сжатый текст, `hash` и `savings_percent`, а полный оригинал при
необходимости достаёт через `headroom_retrieve`.

```json
{
  "compression": {
    "server": "headroom",
    "minBytes": 512,
    "enabled": true,
    "compressTool": "headroom_compress",
    "retrieveTool": "headroom_retrieve"
  },
  "mcpServers": {
    "headroom": {
      "command": "headroom",
      "args": ["mcp", "serve"]
    }
  }
}
```

| Поле | По умолчанию | Описание |
|------|--------------|----------|
| `server` | — | Имя сервера в `mcpServers`, предоставляющего инструменты сжатия. |
| `minBytes` | `512` | Сжимать только результаты не меньше этого размера. |
| `enabled` | `true` | Выключить сжатие, не удаляя блок. |
| `compressTool` | `headroom_compress` | Инструмент сжатия. |
| `retrieveTool` | `headroom_retrieve` | Инструмент восстановления (используется в подсказке модели). |

Поведение:

- Сервер сжатия подключается на старте автоматически, даже если у него
  `autoConnect: false`.
- Инструмент `compressTool` **скрывается** из каталога модели, чтобы она не сжимала
  повторно; `headroom_retrieve` и `headroom_stats` остаются доступными.
- Результаты самого сервера сжатия (например, `headroom_retrieve`) не сжимаются повторно.
- Сжатие **fail-open**: если сервер не подключён, вернул ошибку, ответ не разобрался
  или сжатие не уменьшило число токенов — возвращается исходный результат.
- Промежуточные вызовы внутри `execute_code` не сжимаются — они и так не попадают
  в контекст. Сжимаются только результаты `call_tool`, которые видит модель.
- Итог каждого случая виден в метрике `tokenmaxing_compression_total{outcome}`:
  `compressed`, `skipped_small`, `no_gain`, `tool_error`, `no_peer`, `error`,
  `bad_response`.

### Локальная проверка без Headroom

В репозитории есть заглушка `demo_headroom` с инструментами `headroom_compress`,
`headroom_retrieve` и `headroom_stats`:

```bash
cargo build

cat > /tmp/tokenmaxing-compress.json <<'EOF'
{
  "compression": { "server": "headroom", "minBytes": 20 },
  "mcpServers": {
    "headroom": { "command": "/ABS/PATH/tokenmaxing/target/debug/demo_headroom" },
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "autoConnect": false
    }
  }
}
EOF
```

Подключите шлюз с этим конфигом, вызовите `enable_server` для `filesystem` и прочитайте
достаточно большой файл через `call_tool` — в ответе появится
`structuredContent.headroom` с `compressed`, `hash` и счётчиками токенов.

### Реальный Headroom

```bash
uv tool install "headroom-ai[mcp]"
# шлюз запускает: uvx --python 3.12 --from "headroom-ai[mcp]" headroom mcp serve
```

На реальном Headroom JSON-лог в E2E-прогоне сжался с **23 608 до 12 031 токенов (49%)**,
а `headroom_retrieve` вернул исходные 58 858 символов байт-в-байт. На обычном тексте
Headroom может вернуть `router:noop` (0%) — это нормально; он эффективен на JSON,
логах и результатах поиска. Подробности — в [RESULTS.md](RESULTS.md).

## Метрики

Шлюз отдаёт Prometheus-метрики по HTTP. Адрес задаётся `TOKENMAXING_METRICS_ADDR`
(по умолчанию `127.0.0.1:9464`, значение `off` отключает). Формат — обычный `/metrics`.

```bash
TOKENMAXING_METRICS_ADDR=0.0.0.0:9464 TOKENMAXING_CONFIG=./mcp.json ./target/release/tokenmaxing
curl -s localhost:9464/metrics | grep tokenmaxing_
```

| Метрика | Тип | Что показывает |
|---------|-----|----------------|
| `tokenmaxing_meta_tool_calls_total{tool,status}` | counter | вызовы мета-тулов (`search_tools`, `call_tool`, ...) |
| `tokenmaxing_tool_calls_total{tool,server,status}` | counter | вызовы upstream-тулов |
| `tokenmaxing_tool_call_duration_seconds{tool,server}` | histogram | задержка вызовов upstream |
| `tokenmaxing_code_mode_executions_total{status}` | counter | запуски `execute_code` |
| `tokenmaxing_code_mode_duration_seconds` | histogram | длительность `execute_code` |
| `tokenmaxing_compression_total{outcome}` | counter | итоги авто-сжатия |
| `tokenmaxing_catalog_tools{server}` | gauge | тулов в каталоге по серверу |
| `tokenmaxing_servers_connected{server}` | gauge | 1/0 — подключён ли сервер |
| `tokenmaxing_tool_definition_tokens{server}` | gauge | токены всех определений тулов сервера (baseline «без шлюза») |
| `tokenmaxing_meta_tool_definition_tokens` | gauge | токены 7 мета-тулов шлюза (0 в passthrough) |
| `tokenmaxing_inspection_tokens_total` | counter | токены ответов `search_tools` / `get_tool_details` |
| `tokenmaxing_compression_original_tokens_total` | counter | токены до сжатия Headroom |
| `tokenmaxing_compression_compressed_tokens_total` | counter | токены после сжатия Headroom |
| `tokenmaxing_compression_saved_tokens_total` | counter | сэкономленные токены Headroom |
| `tokenmaxing_tokenizer_info{tokenizer}` | gauge | активная кодировка токенайзера |

Токены считаются **реальным токенайзером tiktoken** (`o200k_base` по умолчанию), а не
`символы / 4`. Кодировку можно выбрать через `TOKENMAXING_TOKENIZER`: `o200k_base`,
`cl100k_base`, `p50k_base`, `r50k_base`. Для конкретной модели провайдера точные цифры
даёт A/B-бенчмарк — см. [RESULTS.md](RESULTS.md).

В passthrough метрики мета-тулов (`tokenmaxing_meta_tool_calls_total`,
`tokenmaxing_meta_tool_definition_tokens`, `tokenmaxing_inspection_tokens_total`)
остаются нулевыми — смотрите `tokenmaxing_tool_calls_total` и
`tokenmaxing_tool_definition_tokens`.

Метрики отдаются в стандартном Prometheus-формате, поэтому подходят для любого
Prometheus/Grafana. Отдельного стека мониторинга в минимальном репозитории нет.

## Экономия токенов

Есть два независимых способа оценить выигрыш.

### 1. Модель стоимости (без LLM)

По метрикам на одном и том же каталоге видно, сколько токенов ушло бы на все
определения тулов при прямом подключении и сколько тратит шлюз:

- direct baseline: `sum(tokenmaxing_tool_definition_tokens)`;
- gateway (discovery): `tokenmaxing_meta_tool_definition_tokens`;
- плюс загруженные по требованию схемы: `tokenmaxing_inspection_tokens_total`;
- плюс экономия сжатия: `tokenmaxing_compression_saved_tokens_total`.

Реальный токенайзер (`o200k_base`), 4 сервера и 42 тула: baseline **19 416** токенов,
мета-тулы шлюза **1 367** — стартовая экономия **93.0%**. После инспекции двух тулов
(2 253 токена ответов) остаётся **81.4%**. С DeepWiki (5 серверов, 47 тулов): baseline
**19 815**, стартовая экономия **93.1%**.

### 2. Реальный A/B на промптах через LLM

Бенчмарк запускал `opencode run --format json` на одних и тех же промптах дважды:
с прямым подключением всех серверов и через `tokenmaxing`, и сравнивал **реальные токены
провайдера** из событий `step_finish` (total/input/output/cache/cost). Скрипт
бенчмарка не входит в минимальный репозиторий; зафиксированные артефакты — в `results/`.

Исторический снимок — **20 промптов** (filesystem, memory, sequential-thinking,
everything). Полная таблица: `results/ab_tokens_table.md`, графики и разбор —
в [RESULTS.md](RESULTS.md).

| | direct | gateway | разница |
|---|---:|---:|---:|
| input (некэш.) | 169 513 | 175 270 | gateway **+3.4%** |
| cost | $0.03235 | $0.03932 | gateway **+21.5%** |
| total (с cache read) | 1 228 334 | 1 220 324 | gateway **−0.7%** |

Ключевой эффект — **кэш определений тулов у провайдера**. На первом промпте
(кэш холодный) шлюз дешевле: input −49.1%, cost −33.1%. Дальше direct получает
cache-hit'ы на свои 47 определений (input падает до 269–1 275), а шлюз платит
за discovery (~8–10k input) — на длинной серии простых задач direct может быть
дешевле. Предыдущий прогон из 3 промптов при более холодном кэше дал −61.7% input
и −45.6% cost. Артефакты: `results/ab_tokens.json`, `results/ab_tokens_table.md`.

**Когда шлюз выигрывает:** большой или меняющийся каталог, несколько upstream,
крупные структурированные результаты, холодный кэш, ограничения по контексту.
**Когда прямой режим может быть проще:** единицы тулов и длинная серия простых
промптов с прогретым кэшем.

Новый снимок (2026-11-26, после гибридного режима и дешёвого discovery):
`search_tools` по умолчанию стал в **4.9 раза** дешевле, типовой discovery-флоу —
на **44.7%** дешевле, `schema` вдвое дешевле `full`. На каталогах из 9–16 тулов
`passthrough` работает как direct и примерно **вдвое дешевле discovery**, но на
прогретом кэше простых задач discovery всё ещё дороже direct. Таблицы и оговорки —
в [RESULTS.md §6](RESULTS.md#6-гибридный-режим-и-дешёвый-discovery-2026-11-26).

## Переменные окружения

| Переменная | По умолчанию | Назначение |
|------------|--------------|------------|
| `TOKENMAXING_CONFIG` | `./mcp.json` | Путь к файлу конфигурации. |
| `TOKENMAXING_MODE` | `auto` | `auto`, `discovery` или `passthrough`. |
| `TOKENMAXING_PASSTHROUGH_MAX_TOOLS` | `15` | Верхняя граница каталога для auto-passthrough (1–10 000). |
| `TOKENMAXING_METRICS_ADDR` | `127.0.0.1:9464` | Адрес HTTP-сервера метрик; `off` или пустая строка отключают. |
| `TOKENMAXING_TOKENIZER` | `o200k_base` | Кодировка tiktoken: `o200k_base`, `cl100k_base`, `p50k_base`, `r50k_base`. Неизвестное значение → `o200k_base` с предупреждением. |

## Поведение и детали

- **Коллизии имён.** Если два сервера отдают тул с одинаковым именем, второй получает
  префикс `server__tool` (например, `demo__echo`); при повторной коллизии добавляется
  суффикс `_2`, `_3` и т.д. Имена стабильны в пределах сессии.
- **Кэш определений.** Каталог хранится в памяти шлюза; повторная выдача определения
  не делает лишний `tools/list`.
- **`list_changed`.** На `notifications/tools/list_changed` от upstream шлюз
  перечитывает список и переиндексирует каталог (`src/upstream.rs`). В passthrough он
  также пересылает уведомление клиенту; в discovery `tools/list` остаётся неизменным.
- **Выбор режима.** `auto` выбирает passthrough только при полностью подключённом
  каталоге не больше порога. Ленивые серверы и ошибки подключения автоматически
  оставляют discovery.
- **Ошибки вызова.** Неизвестный тул или сервер → JSON-RPC `-32002`; вызов на
  отключённом сервере → `-32603`; аргументы не объект → `-32602`. Ошибка
  upstream-тула (`isError`) в `call_tool` проксируется как результат, а не как
  ошибка транспорта.
- **Конкурентность.** Запросы обрабатываются параллельно. Если вы говорите со шлюзом
  вручную через pipe, дожидайтесь ответа на `enable_server` перед `search_tools`;
  MCP-клиенты делают это автоматически.
- **Завершение.** При закрытии stdin шлюз завершается и закрывает дочерние
  upstream-процессы.

## Ограничения

- Поддерживаются только **tools** (resources/prompts не проксируются).
- Нет OAuth/авторизации для HTTP-серверов (только статические заголовки).
- Code mode — только JavaScript.
- `mcp.json` не поддерживает подстановку env-переменных.
- Авто-сжатие работает только при подключённом сервере Headroom (или совместимом
  MCP-сервере с инструментами `headroom_compress` / `headroom_retrieve`).
- Метрики-сервер минимальный: отдаёт `/metrics` по HTTP без аутентификации.
- В passthrough нет мета-тулов и code mode, а ленивые (`autoConnect: false`) серверы
  не подключаются. Для них используйте discovery.
- В минимальной поставке нет Dockerfile, Makefile и готового `mcp.json` — только
  Cargo-сборка и ваш конфиг.

## Диагностика

| Симптом | Вероятная причина | Что делать |
|---------|-------------------|------------|
| `warning: reading config ...` и пустой каталог | `TOKENMAXING_CONFIG` указывает на отсутствующий файл | Проверьте путь; по умолчанию ищется `./mcp.json`. |
| `skipping server 'x': must specify either ...` | В записи нет ровно одного транспорта | Укажите либо `command`, либо `url`. |
| `failed to connect server 'x'` | Команда не в PATH, нет прав или `npx` не скачал пакет | Запустите `command` вручную; проверьте stderr upstream. |
| Клиент видит только 7 тулов (discovery) | Так и задумано | Модель должна вызвать `search_servers`/`search_tools`; вызовы делает клиент. |
| Клиент видит все тулы напрямую | Выбран passthrough | Это ожидаемо для маленького каталога; принудительно включите `TOKENMAXING_MODE=discovery`. |
| В passthrough нет ленивого сервера | Ленивые серверы не подключаются в этом режиме | Используйте `TOKENMAXING_MODE=discovery` и `enable_server`. |
| `gateway ready ... (mode: ...)` показывает не тот режим | Не все серверы подключились или каталог больше порога | Проверьте `autoConnect`, stderr подключения и `TOKENMAXING_PASSTHROUGH_MAX_TOOLS`. |
| `enable_server` вернул ошибку `tools/list failed` | Сервер запустился, но не отдал список тулов | Проверьте совместимость и логи upstream. |
| `metrics: failed to serve on ...` | Порт занят или адрес недоступен | Задайте другой `TOKENMAXING_METRICS_ADDR` или `off`. |
| Сжатие не применяется | Результат меньше `minBytes`, сервер не подключён или нет выигрыша в токенах | Посмотрите `tokenmaxing_compression_total{outcome}`; уменьшите `minBytes`. |
| `execute_code` упал по таймауту | Скрипт дольше 10 с по умолчанию | Увеличьте `timeout_ms` (макс. 60 000) или сократите число вызовов. |
| Два тула с одинаковым именем | Коллизия между серверами | Используйте имя с префиксом `server__tool`. |

## Разработка

Сборка и проверки — через Cargo:

```bash
cargo build --release --locked
cargo test
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
| `src/compression.rs` | Авто-сжатие результатов через `headroom_compress`. |
| `src/code_mode.rs` | Песочница QuickJS и брокер `callTool`. |
| `src/metrics.rs` | Prometheus-метрики, tiktoken и HTTP-экспорт. |
| `src/bin/demo_upstream.rs` | Демо upstream-сервер для тестов. |
| `src/bin/demo_headroom.rs` | Заглушка Headroom для проверки авто-сжатия. |
| `results/` | Зафиксированные результаты A/B, Headroom и графики. |
| `RESULTS.md` | Методика, цифры и выводы. |
| `.claude/agents/tokenmaxing-*.md` | 10 субагентов Claude Code. |
| `.opencode/agent/tokenmaxing-*.md` | 10 субагентов opencode. |
