# Переход с предыдущего MCP-сервера

До `tracker-mcp` агенты работали с Tracker через предыдущий MCP-сервер на TypeScript (HTTP, Docker). У него было 16 инструментов с префиксом `tracker_`, токен с правом записи в переменных окружения сервера, и все инструменты записи были включены всегда. В `tracker-mcp` инструментов больше (по одному на каждую операцию API), они называются как операции справочника, а запись включается явно.

На этой странице: какие инструменты заменяют старые, чем отличаются аргументы, как проверить новый сервер на тестовой очереди и как переключить агентов.

## Таблица соответствия

Все замены входят в наборы по умолчанию (`issues`, `queues`, `boards`, `users`), поэтому `--toolsets` для них не нужен. Инструменты записи регистрируются только на уровне записи профиля ([уровни доступа](tools.md#уровни-доступа)).

| Предыдущий сервер | Запрос к API | `tracker-mcp` | Уровень | Набор |
| --- | --- | --- | --- | --- |
| `tracker_get_issue` | `GET /v3/issues/{id}` | `get_issue` | чтение | `issues` |
| `tracker_search_issues` | `POST /v3/issues/_search` | `search_issues` | чтение | `issues` |
| `tracker_count_issues` | `POST /v3/issues/_count` | `count_issues` | чтение | `issues` |
| `tracker_create_issue` | `POST /v3/issues` | `create_issue` | запись | `issues` |
| `tracker_update_issue` | `PATCH /v3/issues/{id}` | `patch_issue` | запись | `issues` |
| `tracker_list_comments` | `GET /v3/issues/{id}/comments` | `get_comments` | чтение | `issues` |
| `tracker_add_comment` | `POST /v3/issues/{id}/comments` | `issues_add_comment` | запись | `issues` |
| `tracker_list_transitions` | `GET /v3/issues/{id}/transitions` | `get_transitions` | чтение | `issues` |
| `tracker_execute_transition` | `POST /v3/issues/{id}/transitions/{transitionId}/_execute` | `new_transition` | запись | `issues` |
| `tracker_list_queues` | `GET /v3/queues` | `get_queues` | чтение | `queues` |
| `tracker_get_queue` | `GET /v3/queues/{key}` | `get_queue` | чтение | `queues` |
| `tracker_create_queue` | `POST /v3/queues` | `create_queue` | запись | `queues` |
| `tracker_create_board` | `POST /v3/liveBoards` | `post_board` | запись | `boards` |
| `tracker_upload_temporary_attachment` | `POST /v3/attachments` | `temp_attachment` | запись | `issues` |
| `tracker_upload_issue_attachment` | `POST /v3/issues/{id}/attachments` | `post_attachment` | запись | `issues` |
| `tracker_raw_request` | любой путь `/v3/…` | `api_request` | по операции | всегда |

Кроме замен, агенту доступны инструменты, которых у предыдущего сервера не было: связи (`link_issue`, `issues_get_links`), история изменений (`get_changelog`), чеклисты, учёт времени, спринты (`post_sprint`, `start_sprint`, `get_sprints`), пользователи (`get_user_info`, `get_users`) и остальные операции справочника. Найти нужную помогает `api_search`.

## Отличия аргументов

Общие правила:

- **Ключ задачи** — `issueId` (ключ или ID, как раньше `issueIdOrKey`), ключ очереди — `queueId` (раньше `queueKey`).
- **Тело запроса** — аргумент `body` (JSON-объект). Раньше поля задачи, очереди и доски передавались в `fields`, а поля комментария и поиска — аргументами верхнего уровня; теперь всё, что уходит в тело запроса, лежит в `body`. Параметры строки запроса (`expand`, `page`, `perPage`, `isAddToFollowers`) остаются аргументами верхнего уровня.
- **`expand`** — строка через запятую (`"transitions,attachments"`), а не массив.
- **Неизвестный аргумент** — ошибка до отправки запроса: сообщение перечисляет допустимые аргументы. Поля внутри `body` не ограничиваются схемой: локальные и пользовательские поля задач передаются как раньше.
- **Ответ** со списком и заголовками постраничной выдачи оборачивается: `{"result": …, "pagination": {…}}` ([аргументы и ответы](tools.md#аргументы-и-ответы)).

По инструментам:

| Инструмент | Было | Стало |
| --- | --- | --- |
| `get_issue` | `issueIdOrKey`, `expand` (массив) | `issueId`, `expand` (строка), `fields` — какие поля вернуть. |
| `search_issues` | `filter`, `query`, `order`, `keys`, `queue` верхнего уровня; `page`, `perPage` (до 1000) | `body` с одним из условий `filter`, `query`, `keys`, `queue` (а также `query2` или `filterId`) и `order`; `page`, `perPage`, `expand`, `fields` верхнего уровня. Относительная пагинация — `id`; прокрутка больших выборок — `scrollType`, `perScroll`, `scrollTTLMillis`, затем `scrollId` из `pagination.scroll_id`. |
| `count_issues` | `filter`, `query` | `body: {"filter": …}` или `body: {"query": "…"}`. Ответ — число. |
| `create_issue` | `fields` | `body` с теми же полями (`queue` и `summary` обязательны, `links`, `attachmentIds`, `sprint` и т. д.); `notify` — уведомлять ли участников. |
| `patch_issue` | `issueIdOrKey`, `fields` | `issueId`, `body`; `version` — защита от одновременных изменений. Статус по-прежнему меняется переходом. |
| `get_comments` | `issueIdOrKey`, `page`, `perPage`, `order` | `issueId`, `expand`. Параметров постраничной выдачи в справочнике нет: следующая страница — по ссылке из `pagination.links` или через `api_request` с `query: {"perPage": …, "id": …}`. |
| `issues_add_comment` | `text`, `markupType`, `attachmentIds`, `summonees` верхнего уровня, `isAddToFollowers` | `body: {"text", "markupType", "attachmentIds", "summonees", "maillistSummonees"}`, `isAddToFollowers` верхнего уровня. |
| `new_transition` | `transitionId`, `comment`, `fields` | `transitionId`, `body: {"comment": "…", "<поле>": …}`: комментарий и поля экрана перехода (например, `resolution`) — вместе в `body`. |
| `get_queues` | `expand` (массив), `page`, `perPage` | `expand` (строка), `perPage`. Номер страницы — через `api_request` (`query: {"page": 2}`) или по ссылке из `pagination.links`. |
| `get_queue` | `queueKey` | `queueId`, `expand` (например, `workflows`). |
| `create_queue`, `post_board` | `fields` | `body` с теми же полями. |
| `temp_attachment`, `post_attachment` | `filename`, `fileBase64`, `contentType` | `file` — путь к локальному файлу в [разрешённых каталогах](security.md#локальные-файлы), `filename` — имя в Tracker (по умолчанию имя файла). Тип содержимого определяется по расширению. Файл читает сервер, содержимое не проходит через контекст агента. |
| `api_request` | `method`, `path` относительно `/v3`, `query` (только скаляры), `body` | `method`, `path` с версией (`/v3/issues/TEST-1/worklog`), `query` (массив — повторением ключа), `body`, `save_to` для скачивания файлов. Уровень доступа определяется по операции, с которой совпал путь ([подробнее](tools.md#api_search-и-api_request)). |

Временный файл, как и раньше, привязывается к задаче или комментарию по ID: `temp_attachment` возвращает `id`, его передают в `attachmentIds` в `body` у `create_issue`, `patch_issue` (а для файлов в описании — `descriptionAttachmentIds`) или `issues_add_comment`.

Связи задач можно ставить, как раньше, полем `links` в `body` у `create_issue`, а у существующей задачи — отдельным инструментом `link_issue` (`body: {"relationship": "relates", "issue": "<ключ>"}`).

## Что изменилось по поведению

- **Только чтение по умолчанию.** У предыдущего сервера инструменты записи и `tracker_raw_request` с любым методом были доступны всегда. `tracker-mcp` без флагов регистрирует только чтение; агенту, который создаёт и меняет задачи, нужен уровень записи профиля (`connect --allow-write` или `tracker-mcp ui`), а удаление (в том числе `DELETE` через `api_request`) — уровень удаления (`--allow-destructive`). На macOS повышение прав подтверждается Touch ID или паролем.
- **Аннотации.** Изменение задачи и переход по статусу раньше были помечены как разрушающие; теперь это уровень записи, а разрушающими считаются только удаление и необратимые изменения.
- **Токен.** Раньше один токен лежал в переменных окружения сервера и был общим для всех агентов. Теперь у каждого агента свой профиль и токен в системном хранилище ([HTTP-режим](http.md) — токен в заголовке запроса).
- **Файлы.** Раньше файл передавался в аргументе как base64. Теперь сервер читает его с диска по пути, поэтому файл должен лежать в разрешённом каталоге. В [HTTP-режиме](http.md#отличия-от-stdio) локальных файлов нет, и инструменты загрузки не регистрируются: агентам, которые прикладывают файлы, нужен stdio-сервер.
- **Постраничная выдача** приходит в `pagination` вместе с ответом (`total_pages`, `total_count`, `scroll_id`, `links`).
- **Учётные данные** в ответах заменяются на `[redacted]` ([подробнее](security.md#учётные-данные-в-ответах)).

## Проверка на тестовой очереди

Перед переключением проверьте новый сервер на живом API. Записывающие шаги выполняйте в отдельной тестовой очереди: ниже она обозначена `<TEST_QUEUE>`, ключи созданных задач — `<A>` и `<B>`. В Claude Code инструменты сервера `tracker` вызываются как `mcp__tracker__<инструмент>`.

### Только чтение

Сервер по умолчанию: `tracker-mcp connect <клиент>` без флагов.

| # | Вызов | Ожидается |
| --- | --- | --- |
| 1 | `get_user_info` `{}` | Логин пользователя токена. |
| 2 | `api_search` `{"query": "переход по статусу"}` | `get_transitions` (зарегистрирован) и `new_transition` с подсказкой про уровень записи. |
| 3 | `get_queues` `{"perPage": 5}` | Не больше пяти очередей. |
| 4 | `get_queue` `{"queueId": "<TEST_QUEUE>", "expand": "workflows"}` | Очередь и её рабочие процессы. |
| 5 | `search_issues` `{"body": {"queue": "<TEST_QUEUE>"}, "perPage": 5}` | Задачи очереди, `pagination.links` с `rel="next"`. |
| 6 | `search_issues` `{"body": {"query": "Queue: <TEST_QUEUE> Resolution: empty() \"Sort by\": Updated DESC"}, "perPage": 10}` | Открытые задачи, сортировка по обновлению. |
| 7 | `search_issues` `{"body": {"filter": {"queue": "<TEST_QUEUE>"}, "order": "-updated"}, "fields": "key,summary,status"}` | В задачах только запрошенные поля. |
| 8 | `search_issues` `{"body": {"keys": ["<TEST_QUEUE>-1", "<TEST_QUEUE>-2"]}}` | Две задачи. |
| 9 | `search_issues` `{"body": {"filter": {"queue": "<TEST_QUEUE>"}}, "scrollType": "unsorted", "perScroll": 2}`, затем тот же вызов с `"scrollId": "<pagination.scroll_id>"` | Первая и следующая порции; `scroll_id` пропадает на последней. |
| 10 | `count_issues` `{"body": {"query": "Queue: <TEST_QUEUE>"}}` и `{"body": {"filter": {"queue": "<TEST_QUEUE>"}}}` | Одно и то же число. |
| 11 | `get_issue` `{"issueId": "<TEST_QUEUE>-1", "expand": "transitions,attachments"}` | Задача с переходами и файлами. |
| 12 | `get_comments`, `get_transitions`, `issues_get_links`, `get_attachments_list` с `{"issueId": "<TEST_QUEUE>-1"}` | Списки (могут быть пустыми). |
| 13 | `api_request` `{"method": "POST", "path": "/v3/issues/_count", "body": {"query": "Queue: <TEST_QUEUE>"}}` | То же число, что в шаге 10: читающий `POST` разрешён. |
| 14 | `create_issue` `{"body": {"queue": "<TEST_QUEUE>", "summary": "x"}}` | Ошибка инструмента: нужен уровень записи. Задача не создана. |
| 15 | `api_request` `{"method": "POST", "path": "/v3/issues", "body": {"queue": "<TEST_QUEUE>", "summary": "x"}}` | Ошибка: запрос выше уровня сервера не отправляется. |

### Запись

Сервер с уровнем записи: `tracker-mcp connect <клиент> --allow-write`. Для шагов с файлами положите в рабочий каталог сервера (обычно каталог проекта) файл `tracker-mcp-check.txt`.

| # | Вызов | Ожидается |
| --- | --- | --- |
| 16 | `create_issue` `{"body": {"queue": "<TEST_QUEUE>", "summary": "tracker-mcp: проверка", "description": "Создано при проверке **tracker-mcp**", "markupType": "md"}}` | Новая задача `<A>`. |
| 17 | `create_issue` `{"body": {"queue": "<TEST_QUEUE>", "summary": "tracker-mcp: связанная", "links": [{"relationship": "relates", "issue": "<A>"}]}}` | Задача `<B>`, связанная с `<A>`. |
| 18 | `patch_issue` `{"issueId": "<A>", "body": {"summary": "tracker-mcp: проверка (изменено)"}}` | Новое название. |
| 19 | `issues_add_comment` `{"issueId": "<A>", "body": {"text": "Комментарий с **разметкой**", "markupType": "md"}}` | Комментарий с разметкой. |
| 20 | `temp_attachment` `{"file": "tracker-mcp-check.txt"}`, затем `issues_add_comment` `{"issueId": "<A>", "body": {"text": "С файлом", "attachmentIds": ["<id>"]}}` | Файл в комментарии и на вкладке вложений. |
| 21 | `post_attachment` `{"issueId": "<A>", "file": "tracker-mcp-check.txt", "filename": "check-renamed.txt"}`, затем `get_attachments_list` `{"issueId": "<A>"}` | Файл `check-renamed.txt` у задачи. |
| 22 | `post_attachment` `{"issueId": "<A>", "file": "/etc/hosts"}` | Ошибка: путь вне разрешённых каталогов. |
| 23 | `get_transitions` `{"issueId": "<A>"}`, затем `new_transition` `{"issueId": "<A>", "transitionId": "<id>", "body": {"comment": "Взял в работу"}}` | Новый статус и комментарий к переходу. |
| 24 | `new_transition` `{"issueId": "<A>", "transitionId": "<id закрытия>", "body": {"resolution": "fixed", "comment": "Готово"}}` | Задача закрыта с резолюцией. |
| 25 | `link_issue` `{"issueId": "<B>", "body": {"relationship": "depends on", "issue": "<A>"}}`, затем `issues_get_links` `{"issueId": "<B>"}` | Связь «Зависит от». |
| 26 | `api_request` `{"method": "POST", "path": "/v3/issues/<A>/comments", "body": {"text": "Через api_request"}}` | Комментарий с этим текстом: тело запроса доходит до API. |
| 27 | `post_board` `{"body": {"name": "tracker-mcp check", "boardPermissionsTemplate": "private", "autoFilters": {"addFilter": {"enabled": true, "liveFilter": {"fieldValues": {"queue": [{"fixed": "<TEST_QUEUE>"}]}}}}}}` | Новая доска с задачами тестовой очереди. |
| 28 | `create_queue` `{"body": {"key": "<NEW_QUEUE>", "name": "tracker-mcp check", "lead": "<логин>", "defaultType": "task", "defaultPriority": "normal", "issueTypesConfig": [{"issueType": "task", "workflow": "<рабочий процесс из шага 4>", "resolutions": ["fixed"]}]}}` | Новая очередь. Необязательный шаг: удалить очередь можно только на уровне удаления или в интерфейсе Tracker. |
| 29 | `issues_delete_comment` `{"issueId": "<A>", "commentId": "<id>"}` | Ошибка: нужен уровень удаления. |

### Удаление

Необязательно, сервер с уровнем удаления (`connect <клиент> --allow-destructive`): `delete_link_issue`, `issues_delete_comment`, `issues_delete_attachment` и `api_request` с `DELETE` на объектах из шагов выше; доску и очередь из шагов 27 и 28 — `delete_board` и `delete_queue`.

### HTTP-режим

Если агенты будут работать с [HTTP-сервером](http.md), повторите шаги 1, 5, 10, 13 и 16–19 на нём и проверьте, что `temp_attachment` и `post_attachment` не зарегистрированы, а вызов по имени возвращает ошибку о локальных файлах.

## Переключение агентов

### Пробный период

Подключите `tracker-mcp` рядом с предыдущим сервером, под другим именем и только на чтение, с отдельным OAuth-приложением только с `tracker:read` ([OAuth-приложение и токен](oauth.md)):

```bash
tracker-mcp connect claude-code      # имя сервера по умолчанию — tracker
tracker-mcp connect codex
```

Предыдущий сервер остаётся зарегистрирован под своим именем, и агенты пишут через него. Новым сервером агенты читают: ищут и считают задачи, читают задачи, комментарии и переходы. Попросите агентов в этот период пользоваться новым сервером для чтения и сообщать о расхождениях.

### Переключение

1. Сохраните запись предыдущего сервера, чтобы её можно было вернуть: `claude mcp get <старое имя>` (адрес и заголовки) и таблицу `[mcp_servers.<старое имя>]` из `~/.codex/config.toml`.
2. Перерегистрируйте `tracker-mcp` с нужными правами: `tracker-mcp connect <клиент> --allow-write` (повторный `connect` оставит сохранённый токен). Если OAuth-приложение профиля создано только с `tracker:read`, добавьте ему право `tracker:write` и получите новый токен ([права приложения](oauth.md#права-приложения)). Агентам, которые только читают, оставьте сервер и токен только на чтение.
3. Удалите регистрацию предыдущего сервера: `claude mcp remove --scope user <старое имя>`, таблицу `[mcp_servers.<старое имя>]` в `~/.codex/config.toml`.
4. Обновите инструкции агентов и разрешения клиентов:
   - имена инструментов — по [таблице соответствия](#таблица-соответствия), аргументы — по [отличиям](#отличия-аргументов);
   - полные имена в Claude Code: `mcp__<старое имя>__tracker_search_issues` → `mcp__tracker__search_issues`; разрешения в настройках Claude Code (`permissions.allow`) — так же, либо одной записью `mcp__tracker` на все инструменты сервера;
   - обходные пути, которые были нужны из-за ограничений предыдущего сервера (например, «писать только специализированными инструментами»), пересмотрите: `api_request` передаёт тело запроса в любом методе.
5. Перезапустите клиенты.

Чтобы вызовы в инструкциях не пришлось менять, можно зарегистрировать `tracker-mcp` под старым именем (`connect … --name <старое имя>`; запись предыдущего сервера заменяется только после подтверждения в терминале). Но имена инструментов и аргументы всё равно другие, поэтому инструкции обновлять нужно в любом случае, а отдельное имя проще отличить при откате.

### Откат

Верните регистрацию предыдущего сервера из записи, сохранённой на шаге 1 (`claude mcp add --transport http <старое имя> <адрес> --header …`, таблица в `~/.codex/config.toml`), и инструкции агентов. Регистрацию `tracker-mcp` можно оставить на чтение (`tracker-mcp connect <клиент>` без флагов) или удалить: `claude mcp remove --scope user tracker`, таблица `[mcp_servers.tracker]` в Codex.
