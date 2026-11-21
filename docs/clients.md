# Подключение к MCP-клиентам

Сервер работает через stdio: MCP-клиент сам запускает `tracker-mcp serve --profile <профиль>` и общается с ним через stdin и stdout. Токен и организацию сервер берёт из профиля (токен — из системного хранилища, для федеративных и сервисных аккаунтов — IAM-токен от `yc`, см. [IAM-токены](iam.md)), поэтому в конфигурации клиента их нет.

Рекомендуемая схема: **один клиент — один профиль — одно OAuth-приложение и токен**. Тогда токен любого агента можно отозвать или заменить, не трогая остальных, а права настраиваются для каждого клиента отдельно ([OAuth-приложение и токен](oauth.md)).

Если на машине агента нет системного хранилища или клиент подключается к серверу по адресу, используйте [HTTP-режим](http.md#подключение-агентов).

## Claude Code и Codex: connect

```bash
tracker-mcp connect claude-code
tracker-mcp connect codex
```

`connect` по шагам:

1. Помогает создать собственное OAuth-приложение Яндекса и сохраняет его `ClientID` в профиль (только в первый раз).
2. Спрашивает организацию: тип (Yandex 360 или Yandex Cloud) и ID.
3. Открывает ссылку получения токена, принимает токен скрытым вводом и проверяет его запросом `GET /v3/myself`. Если рабочий токен уже сохранён, предлагает его оставить.
4. Сохраняет токен в системное хранилище, а настройки — в профиль `~/.config/tracker-mcp/profiles.toml` (в Windows — `%APPDATA%\tracker-mcp\profiles.toml`, см. [Windows](#windows)).
5. Показывает, что изменится в конфигурации клиента, и после подтверждения регистрирует сервер `tracker`: в Claude Code — через `claude mcp add`, в Codex — таблицей `[mcp_servers.tracker]` в `~/.codex/config.toml`.

После этого перезапустите клиент. Профиль по умолчанию называется как клиент (`claude-code`, `codex`), имя сервера — `tracker`. Все флаги и подробности — в [справочнике CLI](cli.md#connect).

### Права и наборы

Флаги `connect` сохраняются в профиль. Уровень доступа — только в профиле: клиент запускает `serve --profile <профиль>` без флагов доступа, поэтому правка конфигурации клиента прав не добавляет. Наборы (`--toolsets`, `--dynamic-toolsets`) попадают и в команду запуска сервера:

```bash
# Только чтение (по умолчанию)
tracker-mcp connect codex

# Создание и изменение задач, комментарии, переходы
# (на macOS повышение прав подтверждается Touch ID или паролем)
tracker-mcp connect codex --allow-write

# Плюс удаление
tracker-mcp connect codex --allow-destructive

# Дополнительные наборы инструментов
tracker-mcp connect claude-code --toolsets issues,queues,boards,users,filters
```

Чтобы поменять права, запустите `tracker-mcp ui` (вкладка Access) или `connect` ещё раз с нужными флагами: сохранённый токен можно оставить, уровень доступа меняется в профиле, а наборы — в профиле и в записи клиента. Без флагов доступа повторный `connect` права профиля не меняет. Регистрации старых версий с флагами доступа в `args` продолжают работать (флаги только сужают права профиля); повторный `connect` или `f` на вкладке Clients в `tracker-mcp ui` переписывает их без флагов. Для записи OAuth-приложению профиля нужно право `tracker:write` ([права приложения](oauth.md#права-приложения)).

С `--scope project` (только Claude Code) сервер регистрируется в `.mcp.json` текущего каталога. Токен при этом остаётся в хранилище того, кто запускал `connect`, а в команде записан абсолютный путь к бинарнику на его машине: другим участникам проекта нужен свой токен под тем же именем профиля (`tracker-mcp auth login --profile <профиль>`) и бинарник по тому же пути.

### Рядом с другим сервером Tracker

`--name` задаёт имя сервера в клиенте. Так `tracker-mcp` можно подключить рядом с другим MCP-сервером Tracker, не трогая его запись, например на пробный период только на чтение ([Переход с предыдущего сервера](migration.md#переключение-агентов)). Запись с тем же именем, которая запускает не `tracker-mcp`, `connect` заменяет только после явного ответа в терминале.

## Другие клиенты: ручное подключение

`connect` настраивает только Claude Code и Codex. Любой клиент, который умеет запускать stdio MCP-серверы, подключается вручную. Ниже — общий порядок, затем особенности [Google Antigravity](#google-antigravity), [Kimi Code](#kimi-code) и [ZCode](#zcode).

1. Сохраните токен и организацию в отдельный профиль:

   ```bash
   tracker-mcp auth login --profile cursor
   ```

   Новый профиль создаётся с доступом только на чтение. Поднять уровень: `tracker-mcp ui` (профиль в списке, вкладка Access) или `tracker-mcp connect` с `--allow-write`; на macOS это подтверждается Touch ID или паролем. Если в профиле нет `client_id`, получите токен по ссылке вашего приложения ([получение токена](oauth.md#получение-токена)).

2. Узнайте абсолютный путь к бинарнику: `command -v tracker-mcp` (в Windows — `(Get-Command tracker-mcp).Source`). Клиенты, запущенные не из терминала, часто не видят `PATH` оболочки.

3. Зарегистрируйте в клиенте stdio-сервер: команда — путь к `tracker-mcp`, аргументы — `serve --profile <профиль>` и, если нужно, `--toolsets`. Флаги доступа не нужны: права берутся из профиля. Формат `mcpServers`, который понимает большинство клиентов:

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

   Переменные окружения и токен в записи не нужны.

4. Перезапустите клиент и проверьте, что у сервера `tracker` появились инструменты.

Права сервер берёт из профиля (флаги `--allow-write`, `--allow-destructive` и `--allow-path` в `args` их только сужают), наборы — из `--toolsets` или профиля; без настроек — только чтение с наборами по умолчанию. Проверить, с какими настройками запустится сервер: `tracker-mcp serve --status --profile <профиль>`.

### Ручная регистрация в Claude Code и Codex

Если вы не хотите, чтобы `connect` менял конфигурацию, зарегистрируйте сервер сами после `auth login`:

```bash
claude mcp add --scope user tracker -- "$(command -v tracker-mcp)" serve --profile claude-code
```

```toml
# ~/.codex/config.toml
[mcp_servers.tracker]
command = "/opt/homebrew/bin/tracker-mcp"
args = ["serve", "--profile", "codex"]
```

### Google Antigravity

Профиль: `tracker-mcp auth login --profile antigravity`.

Глобальный конфиг MCP — `~/.gemini/config/mcp_config.json`, общий для Antigravity App, IDE и CLI (версия 2.x). Antigravity IDE 1.x читает `~/.gemini/antigravity/mcp_config.json`. Если не уверены, какой файл действует, откройте его из интерфейса: в IDE — «…» на панели агента → MCP Servers → Manage MCP Servers → View raw config, в App — Customizations → Installed → MCP Servers → Copy Config File Path. Конфиг одного проекта — `<проект>/.agents/mcp_config.json`.

```json
{
  "mcpServers": {
    "tracker": {
      "command": "/opt/homebrew/bin/tracker-mcp",
      "args": ["serve", "--profile", "antigravity"]
    }
  }
}
```

После сохранения нажмите Refresh в Manage MCP Servers (в App — Restart у сервера, в CLI — `/mcp`). Сервер `tracker` должен появиться со списком инструментов.

Antigravity не включает больше 100 инструментов. Только на чтение с наборами по умолчанию их 60, а с уровнем записи — 102: сузьте наборы, например `--toolsets issues,queues,users` (87 инструментов), или отключите лишние полем `disabledTools` записи.

### Kimi Code

Профиль: `tracker-mcp auth login --profile kimi`.

Kimi Code CLI читает серверы из `~/.kimi-code/mcp.json` (или `$KIMI_CODE_HOME/mcp.json`), конфиг одного проекта — `.kimi-code/mcp.json`. Запись можно добавить командой `/mcp-config` в сессии или вписать в файл:

```json
{
  "mcpServers": {
    "tracker": {
      "command": "/opt/homebrew/bin/tracker-mcp",
      "args": ["serve", "--profile", "kimi"]
    }
  }
}
```

Сервер появляется только в новой сессии; проверьте его командой `/mcp`. Инструменты в Kimi называются `mcp__tracker__<инструмент>`, вызовы без постоянного разрешения клиент подтверждает.

Предыдущий клиент, kimi-cli, хранит тот же формат в `~/.kimi/mcp.json` и регистрирует сервер командой:

```bash
kimi mcp add --transport stdio tracker -- "$(command -v tracker-mcp)" serve --profile kimi
```

### ZCode

Профиль: `tracker-mcp auth login --profile zcode`.

Сервер добавляется в Settings → MCP servers → New MCP server: область User (все проекты) или Workspace (текущий проект), имя `tracker`, тип stdio, в `command` — абсолютный путь к `tracker-mcp`, в `args` — `serve --profile zcode`. Проверьте, что переключатель сервера включён.

Панель пишет в `~/.zcode/cli/config.json` (проект — `<проект>/.zcode/config.json`). Серверы там лежат под ключом `mcp.servers`, а не `mcpServers`:

```json
{
  "mcp": {
    "servers": {
      "tracker": {
        "command": "/opt/homebrew/bin/tracker-mcp",
        "args": ["serve", "--profile", "zcode"]
      }
    }
  }
}
```

Импорт серверов из Claude Code и Codex на той же странице перенесёт их запись вместе с профилем: агент ZCode будет работать с токеном другого клиента. Чтобы у ZCode был свой токен, добавляйте запись вручную.

## Windows

`connect`, `auth` и `serve` работают в Windows так же, команды можно запускать из PowerShell, `cmd` или Git Bash. Установка — [Установка: Windows](install.md#windows).

- **Токен** хранится в Windows Credential Manager: общие учётные данные `<профиль>.dev.filipov.tracker-mcp` (Панель управления → Диспетчер учётных данных → Учётные данные Windows). Настройки профилей — в `%APPDATA%\tracker-mcp\profiles.toml`. Credential Manager доступен только в сеансе, вошедшем с паролем: в SSH-сеансе со входом по ключу, в службе или задаче планировщика без сохранённого пароля Windows не даёт хранилища (ошибка «A specified logon session does not exist»). Запускайте `connect`, `auth` и MCP-клиент в обычном сеансе пользователя; для агента на другой машине есть [HTTP-режим](http.md).
- **Claude Code**: `connect claude-code` вызывает `claude mcp add`, поэтому команда `claude` должна быть в `PATH` (подходят и `claude.exe` из установщика Claude Code, и `claude.cmd` из npm).
- **Codex**: `connect codex` пишет в `%USERPROFILE%\.codex\config.toml` (или в `config.toml` каталога из `CODEX_HOME`).
- В конфигурацию клиента записывается полный путь к `tracker-mcp.exe`, из которого запущен `connect`, например `C:\Users\<имя>\AppData\Local\Programs\tracker-mcp\tracker-mcp.exe`. Обновляйте бинарник заменой файла по тому же пути, тогда `connect` повторять не нужно.
Ручная регистрация в Codex в Windows (в TOML обратные косые черты внутри одинарных кавычек не экранируются):

```toml
# %USERPROFILE%\.codex\config.toml
[mcp_servers.tracker]
command = 'C:\Users\<имя>\AppData\Local\Programs\tracker-mcp\tracker-mcp.exe'
args = ["serve", "--profile", "codex"]
```

## Рабочий каталог и локальные файлы

Инструменты, которые загружают или сохраняют файлы, работают только с путями внутри рабочего каталога сервера, системного временного каталога и каталогов `allow_path` профиля. Рабочий каталог сервера — тот, в котором клиент его запустил (обычно каталог проекта). Если клиент запускает сервер в домашнем каталоге или в корне файловой системы, рабочий каталог в разрешённые не попадает: добавьте нужные каталоги в профиль через `tracker-mcp ui` (вкладка Access). Подробнее — [Безопасность](security.md#локальные-файлы).

## Несколько профилей

Профилей может быть сколько угодно: например, `claude-code` только на чтение в одной организации и `codex` с правом записи в другой. Проверить профиль:

```bash
tracker-mcp auth status --profile codex
```

Команда показывает, сохранён ли токен (не его значение), организацию, пользователя токена, клиент, разрешены ли запись и удаление и выбранные наборы.

## Отключение

1. Удалите запись `tracker` из клиента: `claude mcp remove --scope user tracker`, таблицу `[mcp_servers.tracker]` из `~/.codex/config.toml` или запись из конфигурации другого клиента.
2. Удалите токен из хранилища и настройки профиля: `tracker-mcp auth logout --profile codex --forget`.
3. Отзовите токен ([отзыв токена](oauth.md#отзыв-токена)).
