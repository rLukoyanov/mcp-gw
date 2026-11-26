# mcp-gw

MCP-шлюз на Rust: подключается к вашим MCP-серверам и отдаёт модели **прогрессивный поиск
инструментов** вместо того, чтобы грузить все определения тулов в контекст сразу.

> Измерения и выводы «где выигрываем»: **[RESULTS.md](RESULTS.md)**.

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

## Метрики

Шлюз отдаёт Prometheus-метрики по HTTP. Адрес задаётся `MCP_GW_METRICS_ADDR`
(по умолчанию `127.0.0.1:9464`, значение `off` отключает). Формат — обычный `/metrics`.

```bash
MCP_GW_METRICS_ADDR=0.0.0.0:9464 MCP_GW_CONFIG=./mcp.json ./target/release/mcp-gw
curl -s localhost:9464/metrics | grep mcp_gw_
```

Метрики:

| Метрика | Тип | Что показывает |
|---------|-----|----------------|
| `mcp_gw_meta_tool_calls_total{tool,status}` | counter | вызовы мета-тулов (`search_tools`, `call_tool`, ...) |
| `mcp_gw_tool_calls_total{tool,server,status}` | counter | вызовы upstream-тулов |
| `mcp_gw_tool_call_duration_seconds{tool,server}` | histogram | задержка вызовов upstream |
| `mcp_gw_code_mode_executions_total{status}` | counter | запуски `execute_code` |
| `mcp_gw_code_mode_duration_seconds` | histogram | длительность `execute_code` |
| `mcp_gw_compression_total{outcome}` | counter | итоги авто-сжатия |
| `mcp_gw_catalog_tools{server}` | gauge | тулов в каталоге по серверу |
| `mcp_gw_servers_connected{server}` | gauge | 1/0 — подключён ли сервер |
| `mcp_gw_tool_definition_tokens{server}` | gauge | токены всех определений тулов сервера (baseline «без шлюза») |
| `mcp_gw_meta_tool_definition_tokens` | gauge | токены 7 мета-тулов шлюза |
| `mcp_gw_inspection_tokens_total` | counter | токены ответов `search_tools` / `get_tool_details` |
| `mcp_gw_compression_original_tokens_total` | counter | токены до сжатия Headroom |
| `mcp_gw_compression_compressed_tokens_total` | counter | токены после сжатия Headroom |
| `mcp_gw_compression_saved_tokens_total` | counter | сэкономленные токены Headroom |

Токены считаются **реальным токенайзером tiktoken** (`o200k_base` по умолчанию).
Кодировку можно выбрать через `MCP_GW_TOKENIZER`:
`o200k_base`, `cl100k_base`, `p50k_base`, `r50k_base`. Текущая кодировка видна в
метрике `mcp_gw_tokenizer_info{tokenizer=...}`. Это уже не `символы / 4`; для
конкретной модели провайдера точные цифры даёт A/B-бенчмарк (результаты в `RESULTS.md`).

Метрики отдаются в стандартном Prometheus-формате; их может скрейпить любой Prometheus.

## Экономия токенов

Есть два способа проверить экономию.

### 1. Модель стоимости (без LLM)

По метрикам на одном и том же каталоге видно, сколько токенов ушло бы на все
определения тулов при прямом подключении и сколько тратит шлюз:

- direct baseline: `sum(mcp_gw_tool_definition_tokens)`
- gateway: `mcp_gw_meta_tool_definition_tokens`
- плюс загруженные по требованию схемы: `mcp_gw_inspection_tokens_total`
- плюс экономия сжатия: `mcp_gw_compression_saved_tokens_total`

Реальный токенайзер (`o200k_base`), набор из `mcp.json` (4 сервера, 42 тула):
baseline **19 416** токенов, мета-тулы шлюза **1 367** — стартовая экономия
**93.0%**; после инспекции двух тулов (2 253 токена ответов) остаётся **81.4%**.
С DeepWiki (5 серверов, 47 тулов): baseline **19 815**, экономия **93.1%**.
Эти числа видны в метриках и на графиках в `RESULTS.md`.

### 2. Реальный A/B на промптах через LLM

Бенчмарк запускал `opencode run --format json` на одних и тех же промптах дважды:
с прямым подключением всех серверов и через `mcp-gw`, и сравнивал **реальные токены
провайдера** из событий `step_finish` (total/input/output/cache/cost). Скрипт
бенчмарка не входит в минимальный репозиторий; зафиксированные артефакты — в `results/`.

Последний снимок — **20 промптов** (filesystem, memory, sequential-thinking,
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
дешевле. Предыдущий 3-промптовый прогон при более холодном кэше дал −61.7% input
и −45.6% cost. Артефакты: `results/ab_tokens.json`, `results/ab_tokens_table.md`, графики в `results/`.

## Конфиг `mcp.json`

Формат совместим с Claude Desktop, плюс два поля: `description` и `autoConnect`.

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

### Готовый набор серверов

Пример набора серверов (создайте `mcp.json` рядом с бинарём; сам файл в минимальный
репозиторий не входит):

| Сервер | Что даёт | Инструментов |
|--------|----------|--------------|
| `filesystem` | чтение/запись/поиск файлов в текущем каталоге | 16 |
| `memory` | граф знаний (сущности и связи) | 9 |
| `sequential-thinking` | пошаговое рассуждение | 1 |
| `everything` | демо-сервер со множеством примеров | 16 |
| `deepwiki` | публичный DeepWiki MCP (docs по GitHub-репозиториям, `url`) | 3 |

Итого ~45 инструментов. `filesystem`/`memory`/`sequential-thinking`/`everything`
подключаются на старте; `deepwiki` — ленивый (`autoConnect: false`), включается через
`enable_server` (публичный HTTPS, без токена). Первый запуск может быть медленнее —
`npx` скачивает пакеты.

Чтобы добавить сервер с секретом (GitHub, поиск и т.п.), допишите запись в `mcp.json`
с `env`/`headers` и при необходимости `autoConnect: false`.

### Агенты (10 + 10)

В репозитории лежат два набора ролей, все работают через MCP-шлюз:

- **Claude Code**: `.claude/agents/gw-*.md` — 10 субагентов (researcher, reviewer,
  planner, debugger, tester, docs-writer, refactorer, security-auditor,
  performance-analyst, release-manager). Claude Code подхватывает их при старте;
  вызов — через делегирование субагентам.
- **opencode**: `.opencode/agent/gw-*.md` — те же 10 ролей с `mode: subagent`.
  После добавления файлов перезапустите opencode; субагенты вызываются через
  инструмент `task` из primary-агента (напрямую `--agent gw-*` нельзя — они subagent).

Все промпты инструктируют сначала искать возможности (`search_servers`/`search_tools`),
читать схему (`get_tool_details`), вызывать (`call_tool`), а для цепочек использовать
`execute_code`; при сжатом результате — `headroom_retrieve`.

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

- Сервер сжатия подключается на старте автоматически, даже если у него `autoConnect: false`.
- Инструмент `compressTool` **скрывается** из каталога модели, чтобы она не сжимала повторно;
  `headroom_retrieve` и `headroom_stats` остаются доступными.
- Результаты самого сервера сжатия (например, `headroom_retrieve`) не сжимаются повторно.
- Сжатие **fail-open**: если сервер не подключён, вернул ошибку или не сэкономил байты,
  возвращается исходный результат.
- Промежуточные вызовы внутри `execute_code` не сжимаются — они и так не попадают в контекст;
  сжимаются только результаты `call_tool`, которые видит модель.

Для локальной проверки без Headroom есть заглушка `demo_headroom`
(`headroom_compress` / `headroom_retrieve` / `headroom_stats`):

```bash
cargo build
# в mcp.json: "compression": { "server": "headroom", "minBytes": 20 }
#             "headroom": { "command": "<repo>/target/debug/demo_headroom" }
```

### Тест с реальным Headroom

Отдельного E2E-скрипта в минимальном репозитории нет; ниже — зафиксированный
прогон. Проверялось:

- результат сжат и содержит `structuredContent.headroom` (hash, токены);
- `headroom_compress` скрыт из каталога, `headroom_retrieve` доступен;
- `headroom_retrieve` возвращает исходный контент байт-в-байт;
- метрика `mcp_gw_compression_saved_tokens_total` выросла.

Установка реального Headroom:

```bash
uv tool install "headroom-ai[mcp]"
# шлюз запускает: uvx --python 3.12 --from "headroom-ai[mcp]" headroom mcp serve
```

Пример прогона на реальном Headroom: JSON-лог **23 608 → 12 031 токенов
(49%)**, `headroom_retrieve` вернул исходные 58 858 символов. На обычном
тексте Headroom может вернуть `router:noop` (0%) — это нормально; он эффективен
на JSON, логах и результатах поиска.

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
- Авто-сжатие работает только при подключённом сервере Headroom (или совместимом
  MCP-сервере с инструментами `headroom_compress` / `headroom_retrieve`).

## Разработка

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
| `src/bin/demo_upstream.rs` | Демо upstream-сервер для тестов. |
| `src/bin/demo_headroom.rs` | Заглушка Headroom для проверки авто-сжатия. |
| `results/` | Зафиксированные результаты A/B, Headroom и графики. |
| `.claude/agents/gw-*.md` | 10 субагентов Claude Code. |
| `.opencode/agent/gw-*.md` | 10 субагентов opencode. |
