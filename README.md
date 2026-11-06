# tracker-mcp

MCP-сервер и CLI для [Yandex Tracker REST API](https://yandex.ru/support/tracker/ru/api/about-api). Даёт Claude Code, Codex и другим MCP-клиентам искать и читать задачи, создавать и менять их, комментировать, переводить по статусам, прикладывать файлы и работать с очередями, досками и спринтами Tracker.

- **Все операции API — MCP-инструменты.** Они генерируются при сборке из OpenAPI-спецификации, собранной из справочника Tracker API; `api_search` находит нужную операцию, `api_request` вызывает любую в пределах уровня доступа.
- **Токен — только в системном хранилище** (macOS Keychain, Secret Service в Linux, Credential Manager в Windows), отдельный профиль и токен для каждого агента. Токен выдаёт ваше собственное OAuth-приложение Яндекса; ни переменные окружения, ни `.env`, ни конфигурация клиента его не содержат. Исключение — HTTP-режим (ниже): там токен агента приходит в заголовке каждого запроса и сервер его не хранит.
- **По умолчанию только чтение.** Запись и удаление включаются явно; инструменты выше разрешённого уровня агенту не видны.
- **Учётные данные не попадают к агенту.** Пароли, токены и секреты интеграций в ответах Tracker заменяются на `[redacted]`.

Сервер работает через stdio: MCP-клиент сам запускает `tracker-mcp serve`. Для Linux-машин без системного хранилища и клиентов, которые подключаются по адресу, есть [HTTP-режим](docs/http.md) (`serve --http`, в том числе в Docker).

## Установка

### Установочный скрипт (Linux и macOS)

```bash
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/tracker-mcp/tarball/refs/heads/install | tar -xzO | sh
```

Скрипт ставит последнюю версию в `~/.local/bin` без `sudo` и сверяет архив с `SHA256SUMS` релиза; повторный запуск обновляет бинарник. Параметры (`--version`, `--prefix`) — в [docs/install.md](docs/install.md#установочный-скрипт).

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/tracker-mcp
```

Tap размещён на SourceCraft, поэтому `brew tap` нужен полный адрес. Устанавливайте формулу по полному имени `filipov-dev/tap/tracker-mcp`.

### Готовый архив

Архивы для macOS (универсальный, подписан и нотаризован), Linux x86_64 и aarch64 (статические, без зависимостей) и `SHA256SUMS` — на [странице релизов](https://sourcecraft.dev/filipov-dev/tracker-mcp/releases).

### Из исходников

```bash
cargo build --release
```

Бинарник: `target/release/tracker-mcp`. Нужен Rust 1.88 или новее и C-компилятор.

Подробнее, включая проверку контрольных сумм, обновление и удаление: [docs/install.md](docs/install.md).

## Быстрый старт: Claude Code и Codex

```bash
tracker-mcp connect claude-code
# или
tracker-mcp connect codex
```

`connect` поможет создать собственное OAuth-приложение Яндекса (откроет форму и подскажет настройки), спросит организацию (Yandex 360 или Yandex Cloud), откроет ссылку получения токена, примет токен скрытым вводом, проверит его и сохранит в системное хранилище под профилем с именем клиента. Затем покажет, что изменится в конфигурации клиента, и после подтверждения зарегистрирует сервер `tracker`. Перезапустите клиент.

Права задаются при подключении и попадают в команду запуска сервера:

```bash
tracker-mcp connect codex --allow-write
```

Агенту, которому достаточно чтения, создайте приложение только с правом `tracker:read`: даже если сервер работает только на чтение, токен с правом записи опасен при утечке. Подробнее: [docs/oauth.md](docs/oauth.md).

## Другие MCP-клиенты

`connect` настраивает только Claude Code и Codex. Любой другой клиент со stdio-серверами подключается вручную:

1. Сохраните токен и организацию в отдельный профиль: `tracker-mcp auth login --profile cursor`.
2. Зарегистрируйте в клиенте stdio-сервер с командой `tracker-mcp` и аргументами `serve --profile <профиль>`:

   ```json
   {
     "mcpServers": {
       "tracker": {
         "command": "/opt/homebrew/bin/tracker-mcp",
         "args": ["serve", "--profile", "cursor"]
       }
     }
   }
   ```

   Укажите в `command` абсолютный путь из `command -v tracker-mcp`. Чтобы разрешить запись или удаление, добавьте в `args` `"--allow-write"` или `"--allow-destructive"`.

Пути к конфигурации и особенности Google Antigravity, Kimi Code и ZCode — в [docs/clients.md](docs/clients.md#google-antigravity).

## HTTP-режим

```bash
docker compose up --build -d   # или: tracker-mcp serve --http
claude mcp add --transport http tracker http://127.0.0.1:8766/mcp \
  --header "Authorization: OAuth <токен>" \
  --header "X-Org-ID: <ID организации>"
```

Сервер ничего не хранит: токен и организация приходят в каждом запросе, токен уходит только в Tracker API. Токен виден агенту в конфигурации клиента, поэтому граница безопасности здесь — права токена, а флаги доступа сервера — страховка от ошибок. Локальные файлы в HTTP-режиме выключены. Подробнее: [docs/http.md](docs/http.md).

## Уровни доступа

| Уровень | Как включить | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Только инструменты, которые читают данные, в том числе поиск и подсчёт задач. |
| Запись | `--allow-write` | Создание и изменение: задачи, комментарии, переходы, связи, файлы, очереди, доски, спринты. |
| Удаление | `--allow-destructive` (включает запись) | Удаление и необратимые изменения, в том числе прав доступа. |

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` выбирает разделы API: по умолчанию включены задачи, очереди, доски и пользователи (`issues`, `queues`, `boards`, `users`), `--toolsets all` включает все. Подробнее: [docs/tools.md](docs/tools.md).

## Переход с предыдущего сервера

Если агенты работали с Tracker через предыдущий MCP-сервер на TypeScript (инструменты `tracker_get_issue`, `tracker_search_issues`, `tracker_raw_request` и другие), соответствие инструментов, отличия аргументов и порядок переключения — в [docs/migration.md](docs/migration.md).

## Документация

- [Установка и обновление](docs/install.md)
- [OAuth-приложение и токен](docs/oauth.md)
- [Подключение к MCP-клиентам](docs/clients.md)
- [HTTP-режим](docs/http.md)
- [Справочник CLI](docs/cli.md)
- [MCP-инструменты](docs/tools.md)
- [Безопасность](docs/security.md)
- [Решение проблем](docs/troubleshooting.md)
- [Переход с предыдущего сервера](docs/migration.md)
- [Разработка](docs/development.md)

## Лицензия

`MIT OR Apache-2.0` — на ваш выбор: [LICENSE-MIT](LICENSE-MIT) или [LICENSE-APACHE](LICENSE-APACHE).
