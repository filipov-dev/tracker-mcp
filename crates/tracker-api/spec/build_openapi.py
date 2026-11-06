#!/usr/bin/env python3
"""Собирает OpenAPI 3.1 для Yandex Tracker API из справочника API.

Официальной спецификации у Tracker нет, поэтому источник — markdown-версии
страниц справочника https://yandex.ru/support/tracker/ru/api/about-api.
Список страниц берётся из llms.txt.

Использование:
    python3 crates/tracker-api/spec/build_openapi.py [--cache <каталог>] [--offline] [--check] [--out crates/tracker-api/spec/tracker.openapi.json]

Без --offline страницы скачиваются в каталог кеша (без --cache — во временный
каталог); с --offline используется уже скачанное. Отчёт о том, что не удалось
разобрать, печатается в stderr.

С --check файл --out не перезаписывается: свежая сборка сравнивается с ним и
печатается разница (страницы, операции, параметры). Код выхода: 0 — изменений
нет, 3 — спецификация отстала от справочника, 1 — ошибка сети или разбора.

Расхождения справочника с реальным API исправляются исключениями в разделе
«исключения» ниже (RESPONSE_SHAPES, BODY_REQUIRED и соседние таблицы), а не правкой JSON.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import sys
import tempfile
import urllib.request
from datetime import date

DOCS_BASE = "https://yandex.ru/support/tracker/ru/api/"
LLMS_URL = "https://yandex.ru/support/tracker/ru/llms.txt"
API_SERVER = "https://api.tracker.yandex.net"

# Разделы справочника -> теги OpenAPI (в порядке вывода).
TAGS = [
    ("issues", "Задачи", "Задачи, комментарии, связи, чеклисты, файлы, учёт времени, поля и отчёты."),
    ("queues", "Очереди", "Очереди, локальные поля, версии, компоненты, триггеры, автодействия и рабочие процессы."),
    ("macros", "Макросы", "Макросы очередей."),
    ("entities", "Проекты, портфели и цели", "Сущности project, portfolio и goal."),
    ("projects", "Проекты (старый API)", "Устаревший API проектов."),
    ("boards", "Доски и спринты", "Доски, колонки и спринты."),
    ("dashboards", "Дашборды", "Дашборды и виджеты."),
    ("filters", "Фильтры", "Сохранённые фильтры задач."),
    ("bulkchange", "Массовые операции", "Массовое изменение задач."),
    ("import", "Импорт", "Импорт задач и их данных из других систем."),
    ("admin", "Администрирование", "Типы задач, приоритеты, резолюции и статусы."),
    ("users", "Пользователи", "Пользователи организации."),
    ("gaps", "Отсутствия", "Отсутствия сотрудников."),
]
TAG_NAME = {key: name for key, name, _ in TAGS}

# Имя параметра пути по предыдущему сегменту адреса. В справочнике одни и те же
# параметры называются по-разному ({id}, {id_задачи}, {issueId}), а OpenAPI не
# допускает одинаковые шаблоны пути с разными именами параметров.
PATH_PARAM_BY_SEGMENT = {
    "issues": "issueId",
    "queues": "queueId",
    "boards": "boardId",
    "columns": "columnId",
    "sprints": "sprintId",
    "comments": "commentId",
    "attachments": "fileId",
    "thumbnails": "fileId",
    "checklistItems": "checklistItemId",
    "links": "linkId",
    "remotelinks": "remoteLinkId",
    "worklog": "worklogId",
    "fields": "fieldId",
    "categories": "categoryId",
    "transitions": "transitionId",
    "reactions": "reactionName",
    "dashboards": "dashboardId",
    "widgets": "widgetId",
    "filters": "filterId",
    "bulkchange": "bulkchangeId",
    "projects": "projectId",
    "entities": "entityType",
    "macros": "macroId",
    "triggers": "triggerId",
    "autoactions": "autoactionId",
    "logs": "runId",
    "versions": "versionId",
    "components": "componentId",
    "groups": "groupId",
    "users": "userId",
    "workflows": "workflowId",
    "steps": "status",
    "actions": "actionId",
    "localFields": "fieldKey",
    "issuetypes": "issueTypeId",
    "priorities": "priorityId",
    "resolutions": "resolutionId",
    "statuses": "statusId",
}
# Параметр сразу после другого параметра.
PATH_PARAM_AFTER_PARAM = {"entityType": "entityId", "fileId": "fileName"}

# POST-методы, которые только читают данные.
READ_ONLY_POST_SUFFIXES = ("/_search", "/_count", "/_list", "/_suggest", "/_paginate", "/_relative", "/scroll/_clear")
# Изменения, которые считаем разрушительными, хотя это не DELETE.
DESTRUCTIVE_PATTERNS = [
    ("POST", re.compile(r"/tags/_remove$")),
    ("PATCH", re.compile(r"/(extendedP|p)ermissions$")),
]

# ---------- исключения: где справочник расходится с реальным API ----------
# Всё ниже проверено запросами на чтение к реальному Tracker API (см. раздел
# «Проверено на реальном API» в README.md). Ключ — (метод, нормализованный путь).

# Форма успешного ответа. "array"/"object" — обернуть или развернуть схему из
# справочника; словарь — заменить схему целиком.
RESPONSE_SHAPES: dict[tuple[str, str], str | dict] = {
    # В справочнике пример — один объект, API возвращает массив полей.
    ("GET", "/v3/fields"): "array",
    ("GET", "/v3/queues/{queueId}/localFields"): "array",
    # Справочник описывает один пункт, API возвращает массив пунктов чеклиста.
    ("GET", "/v3/issues/{issueId}/checklistItems"): "array",
    ("GET", "/v3/issues/{issueId}/comments/{commentId}"): "object",
    ("POST", "/v3/issues/{issueId}/links/_list"): "object",
    # Ответ — число без обёртки.
    ("POST", "/v3/issues/_count"): {"type": "integer", "description": "Число задач, подходящих под условия запроса."},
    ("GET", "/v3/queues/{queueId}/tags"): {"type": "array", "items": {"type": "string"}, "description": "Теги очереди."},
    # Объект «идентификатор рабочего процесса -> типы задач, к которым он применяется».
    ("GET", "/v3/queues/{queueId}/workflows"): {
        "type": "object",
        "description": "Ключ — идентификатор рабочего процесса, значение — типы задач очереди, к которым он применяется.",
        "additionalProperties": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "self": {"type": "string", "description": "Адрес ресурса API типа задачи."},
                    "id": {"type": "string", "description": "Идентификатор типа задачи."},
                    "key": {"type": "string", "description": "Ключ типа задачи."},
                    "display": {"type": "string", "description": "Отображаемое название типа задачи."},
                },
            },
        },
    },
}

# Ответы-обёртки: таблица справочника перечисляет поля элементов массива вперемешку
# с полями обёртки. (метод, путь) -> (свойство-массив, поля самой обёртки).
RESPONSE_WRAPPERS = {
    ("POST", "/v3/entities/{entityType}/_search"): ("values", ("hits", "pages")),
    ("POST", "/v3/entities/report/_search"): ("values", ("hits", "pages", "orderBy")),
    ("GET", "/v3/users/_relative"): ("users", ("hasNext",)),
}

# Типы отдельных полей ответа: (метод, префикс пути) -> {поле: тип}.
RESPONSE_FIELD_TYPES = {
    # Старый API проектов отдаёт id строкой ("2"), справочник пишет «Число».
    ("*", "/v3/projects"): {"id": "string"},
}

# Поля ответа, которых нет в таблицах справочника, но которые нужны клиенту.
RESPONSE_EXTRA_FIELDS = {
    ("GET", "/v3/issues/{issueId}/transitions"): {
        "screen": {
            "type": "object",
            "description": "Экран перехода. Есть, если при переходе можно или нужно заполнить поля задачи.",
        },
    },
}

# Обязательность полей тела запроса: (метод, путь) -> {поле: обязательно ли}.
BODY_REQUIRED = {
    # Таблица называет comment обязательным, но текст той же страницы говорит, что поля
    # передаются, только если настройки перехода это позволяют; переходы без экрана
    # выполняются с пустым телом.
    ("POST", "/v3/issues/{issueId}/transitions/{transitionId}/_execute"): {"comment": False},
    # Нужен хотя бы один из filter и query (примечание на странице справочника).
    ("POST", "/v3/queues/{queueId}/autoactions"): {"filter": False, "query": False},
}

# Тело запроса целиком.
BODY_SCHEMAS = {
    # Таблица описывает поля scrollId и scrollToken, а на деле тело — объект
    # «идентификатор прокрутки -> токен» (так и в примере справочника); API проверяет
    # подпись по ключам объекта.
    ("POST", "/v3/system/search/scroll/_clear"): {
        "type": "object",
        "description": (
            "Пары «значение X-Scroll-Id -> значение X-Scroll-Token» из ответов на поиск задач "
            "с прокруткой, по одной на каждую полученную страницу."
        ),
        "additionalProperties": {"type": "string"},
        "minProperties": 1,
    },
}

# Параметры запроса: (метод, путь) -> {имя: замена схемы}.
QUERY_PARAM_SCHEMAS = {
    ("POST", "/v3/issues/_search"): {
        "scrollType": {"type": "string", "enum": ["sorted", "unsorted"]},
        "perScroll": {"type": "integer", "minimum": 1, "maximum": 1000},
        "scrollTTLMillis": {"type": "integer", "minimum": 1},
        # Справочник пишет «Число», но значения — строки из заголовков X-Scroll-Id и Link.
        "scrollId": {"type": "string"},
        "id": {"type": "string"},
    },
}

# Описания параметров пути, уточнённые по ответам API.
PATH_PARAM_NOTES = {
    # Не шестнадцатеричное значение API отклоняет с 400 «Incorrect data format».
    "runId": "Идентификатор запуска автодействия: 24 шестнадцатеричных символа.",
}

# Версия объекта в параметре запроса version вместо заголовка If-Match.
#
# Часть страниц (спринты, колонки досок) передаёт версию только заголовком If-Match,
# а генератор операций не передаёт заголовки, кроме авторизации и организации: без
# параметра агент не может изменить объект. API принимает версию и параметром version
# (страницы изменения рабочего процесса и прав дашборда прямо говорят «в параметре
# version или заголовке If-Match», остальные методы изменения API v3 используют
# version), поэтому страницам с If-Match параметр добавляется автоматически.
# Здесь — уточнения: (метод, путь) -> {"required": обязателен ли, "note": описание
# для страниц, где If-Match не упомянут}.
VERSION_QUERY_PARAMS: dict[tuple[str, str], dict] = {
    # Изменение спринта без версии API отклоняет (замечено при работе с API; здесь не
    # проверялось: для проверки нужен изменяющий запрос).
    ("PATCH", "/v3/sprints/{sprintId}"): {"required": True},
    # Справочник версию не описывает, но перечисляет ответы 412 и 428, а страницы колонок
    # передают в If-Match именно версию доски.
    ("PATCH", "/v3/boards/{boardId}"): {
        "note": (
            "Текущая версия доски: поле version из ответа на запрос информации о доске. "
            "Справочник версию у этого метода не описывает, но перечисляет ответы 412 и 428."
        ),
    },
}
VERSION_QUERY_NOTE = "Справочник передаёт версию заголовком If-Match; API принимает её и в параметре запроса version."

# Заголовки постраничной выдачи (из справочника; см. «Не проверено» в README.md).
PAGINATION_HEADERS = ("XTotalPages", "XTotalCount")
SCROLL_HEADERS = ("Link", "XScrollId", "XScrollToken", "XTotalCount")
RESPONSE_HEADERS = {
    "XTotalPages": {"description": "Общее количество страниц при постраничной выдаче (page, perPage).", "schema": {"type": "integer"}},
    "XTotalCount": {"description": "Общее количество записей.", "schema": {"type": "integer"}},
    "Link": {
        "description": (
            "Ссылки на первую и следующую страницы (rel=\"first\", rel=\"next\") при относительной "
            "пагинации и прокрутке. Следующая страница — параметры id или scrollId из ссылки."
        ),
        "schema": {"type": "string"},
    },
    "XScrollId": {"description": "Идентификатор следующей страницы прокрутки: значение параметра scrollId.", "schema": {"type": "string"}},
    "XScrollToken": {
        "description": "Токен прокрутки. В поиске не нужен; нужен, чтобы освободить ресурсы прокрутки.",
        "schema": {"type": "string"},
    },
}
HEADER_NAMES = {"XTotalPages": "X-Total-Pages", "XTotalCount": "X-Total-Count", "Link": "Link", "XScrollId": "X-Scroll-Id", "XScrollToken": "X-Scroll-Token"}


def apply_response_overrides(method: str, path: str, schema: dict) -> dict:
    key = (method, path)
    shape = RESPONSE_SHAPES.get(key)
    if isinstance(shape, dict):
        schema = json.loads(json.dumps(shape))
    elif shape == "array" and schema.get("type") != "array":
        schema = {"type": "array", "items": schema}
    elif shape == "object" and schema.get("type") == "array":
        schema = schema.get("items") or {"type": "object"}
    if shape == "object":
        schema.setdefault("type", "object")
    if key in RESPONSE_WRAPPERS:
        array_prop, own = RESPONSE_WRAPPERS[key]
        props = schema.get("properties", {})
        moved = {k: v for k, v in props.items() if k != array_prop and k not in own}
        wrapper = {k: v for k, v in props.items() if k == array_prop or k in own}
        arr = wrapper.setdefault(array_prop, {"type": "array"})
        arr["type"] = "array"
        items = arr.setdefault("items", {"type": "object"})
        items.setdefault("type", "object")
        items.setdefault("properties", {}).update(moved)
        schema = dict(schema, type="object", properties=wrapper)
    target = schema.get("items", schema)
    for (m, prefix), fields in RESPONSE_FIELD_TYPES.items():
        if m in ("*", method) and (path == prefix or path.startswith(prefix + "/")):
            for name, typ in fields.items():
                if name in target.get("properties", {}):
                    target["properties"][name]["type"] = typ
    for name, extra in RESPONSE_EXTRA_FIELDS.get(key, {}).items():
        target.setdefault("properties", {}).setdefault(name, extra)
    return schema


def apply_body_overrides(method: str, path: str, schema: dict) -> dict:
    key = (method, path)
    if key in BODY_SCHEMAS:
        return json.loads(json.dumps(BODY_SCHEMAS[key]))
    for name, required in BODY_REQUIRED.get(key, {}).items():
        names = [n for n in schema.get("required", []) if n != name]
        if required:
            names.append(name)
        if names:
            schema["required"] = names
        else:
            schema.pop("required", None)
    return schema


METHOD_BLOCK_RE =re.compile(r'method_(\w+)">\s*<p>\w+</p>\s*<pre><code>([^<]+)</code>')
FALLBACK_METHOD_RE = re.compile(r"^(GET|POST|PUT|PATCH|DELETE) (/v[23]/\S+)", re.M)
CUT_RE = re.compile(r'\{%\s*cut\s+"([^"]+)"\s*%\}|\{%\s*endcut\s*%\}')
PLACEHOLDER_RE = re.compile(r"^[А-ЯЁ_]+$")
NAME_RE = re.compile(r"^[^\W\d][\w.\-$]*$")


# ---------- загрузка ----------

def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "tracker-openapi-builder"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        return resp.read().decode("utf-8")


def page_urls(llms: str) -> list[str]:
    urls = re.findall(r"https://yandex\.ru/support/tracker/ru/api/[^\s)]+\.md", llms)
    if not urls:
        # Вместо оглавления пришло что-то другое (капча, страница ошибки): пустая
        # спецификация выглядела бы как удаление всех операций.
        raise SourceError("в llms.txt нет страниц справочника API")
    return sorted(set(urls))


class SourceError(Exception):
    """Справочник недоступен или отдал не то, что ожидается."""


def check_markdown(url: str, text: str) -> str:
    head = text.lstrip()[:200].lower()
    if head.startswith("<!doctype") or head.startswith("<html"):
        raise SourceError(f"{url}: вместо markdown получен HTML (капча или ошибка сайта)")
    return text


def cache_name(url: str) -> str:
    return url[len(DOCS_BASE):].replace("/", "__")


def page_name(cache: str) -> str:
    """Имя страницы в отчётах и в info.x-source-pages: «boards__patch-sprint.md» -> «boards/patch-sprint»."""
    return cache.replace("__", "/").removesuffix(".md")


def load_pages(cache: str, offline: bool) -> dict[str, str]:
    os.makedirs(cache, exist_ok=True)
    llms_path = os.path.join(cache, "llms.txt")
    if not offline:
        with open(llms_path, "w", encoding="utf-8") as fh:
            fh.write(check_markdown(LLMS_URL, fetch(LLMS_URL)))
    with open(llms_path, encoding="utf-8") as fh:
        urls = page_urls(fh.read())

    def get(url: str) -> tuple[str, str]:
        path = os.path.join(cache, cache_name(url))
        if not offline:
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(check_markdown(url, fetch(url)))
        with open(path, encoding="utf-8") as fh:
            return cache_name(url), fh.read()

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        return dict(pool.map(get, urls))


# ---------- разбор markdown ----------

def strip_front_matter(text: str) -> str:
    if text.startswith("---"):
        end = text.find("\n---", 3)
        if end != -1:
            return text[end + 4:]
    return text


def clean_text(text: str) -> str:
    """Убирает разметку YFM/HTML, оставляя читаемый текст."""
    text = re.sub(r"<!--.*?-->", "", text, flags=re.S)
    text = re.sub(r"\{%.*?%\}", "", text, flags=re.S)
    text = re.sub(r"<br\s*/?>", "\n", text)
    text = re.sub(r"</li>\s*<li>", "\n- ", text)
    text = re.sub(r"<li>", "- ", text)
    text = re.sub(r"<[^>]+>", "", text)
    text = re.sub(r"\[([^\]]+)\]\((?:\*|#)[^)]*\)", r"\1", text)  # внутренние ссылки и сноски
    text = re.sub(r"\[([^\]]+)\]\(\.{0,2}/?([^)]+?)\.md(#[^)]*)?\)", r"\1", text)  # относительные ссылки
    text = re.sub(r"\{#[^}]*\}", "", text)
    text = re.sub(r"[ \t]+", " ", text)
    text = re.sub(r"\n\s*\n+", "\n", text)
    return text.strip()


def clean_name(cell: str) -> str:
    name = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", cell)
    name = name.replace("`", "").replace("*", "").strip()
    name = re.sub(r"\s*\(.*\)$", "", name)  # «name (обязательный)»
    return name.strip()


def sections(text: str) -> list[tuple[str, str]]:
    """Возвращает блоки {% cut "Заголовок" %}…{% endcut %} верхнего уровня."""
    result, stack = [], []
    for m in CUT_RE.finditer(text):
        if m.group(1) is not None:
            stack.append((m.group(1), m.end()))
        elif stack:
            title, start = stack.pop()
            if not stack:
                result.append((title, text[start:m.start()]))
    return result


def split_pipe_row(line: str) -> list[str]:
    line = line.strip()
    if line.startswith("|"):
        line = line[1:]
    if line.endswith("|"):
        line = line[:-1]
    return [c.strip() for c in line.split("|")]


def parse_tables(block: str) -> list[dict]:
    """Находит таблицы и контекст перед каждой: подзаголовок и объект, к которому они относятся.

    Поддерживает обычные markdown-таблицы и многострочные таблицы YFM (#| … |#).
    """
    tables = []
    lines = block.split("\n")
    required = None
    target = None
    # «Поля объектов массива `gaps`», а под ним «Обязательные»/«Дополнительные»:
    # такие подзаголовки относятся к объекту, а не к верхнему уровню.
    target_pending = False
    sticky = False
    i = 0
    while i < len(lines):
        raw = lines[i]
        line = raw.strip()
        bold = re.match(r"^\*\*([^*]+)\*\*\s*(`([^`]+)`)?", line)
        if bold:
            head = bold.group(1).strip().lower()
            if head.startswith("обязательные") or head.startswith("дополнительные") or head.startswith("общие"):
                required = head.startswith("обязательные")
                if target_pending:
                    sticky = True
                if not sticky:
                    target = None
            elif re.match(r"пол[ея]\b", head) and bold.group(3):
                # «Поля объекта `x`» и «Поле объекта `x`» (так в создании спринта).
                target = bold.group(3).strip()
                target_pending, sticky = True, False
                required = None
            i += 1
            continue
        if line.startswith("#|") or ("|" in line and i + 1 < len(lines) and re.match(r"^\s*\|?\s*-{3,}", lines[i + 1])):
            target_pending = False
        if line.startswith("#|"):
            j = i
            buf = []
            while j < len(lines):
                buf.append(lines[j])
                if lines[j].strip().endswith("|#") and (j > i or lines[j].strip() != "#|"):
                    break
                j += 1
            body = "\n".join(buf).strip()
            body = body[2:]
            if body.endswith("|#"):
                body = body[:-2]
            rows = [r for r in body.split("||") if r.strip()]
            parsed = [[c.strip() for c in r.split("|")] for r in rows]
            if parsed:
                tables.append({"header": parsed[0], "rows": parsed[1:], "required": required, "target": target})
            i = j + 1
            continue
        if "|" in line and i + 1 < len(lines) and re.match(r"^\s*\|?\s*-{3,}", lines[i + 1]):
            header = split_pipe_row(line)
            rows = []
            j = i + 2
            while j < len(lines) and "|" in lines[j] and lines[j].strip():
                rows.append(split_pipe_row(lines[j]))
                j += 1
            tables.append({"header": header, "rows": rows, "required": required, "target": target})
            i = j
            continue
        i += 1
    return tables


def map_type(type_text: str) -> dict:
    t = clean_text(type_text).lower()
    if not t:
        return {}
    if t.startswith("массив"):
        if "строк" in t and "объект" not in t and "чис" not in t:
            return {"type": "array", "items": {"type": "string"}}
        if "объект" in t and "строк" not in t and "чис" not in t:
            return {"type": "array", "items": {"type": "object"}}
        if ("чис" in t) and "строк" not in t and "объект" not in t:
            return {"type": "array", "items": {"type": "number"}}
        return {"type": "array"}
    if t.startswith("строка") or t.startswith("дата") or t.startswith("время"):
        return {"type": "string"}
    if t.startswith("целое"):
        return {"type": "integer"}
    if t.startswith("число"):
        return {"type": "number"}
    if t.startswith("логическ"):
        return {"type": "boolean"}
    if t.startswith("объект") and " или " not in t and "," not in t:
        return {"type": "object"}
    return {}


def table_rows(table: dict) -> list[dict]:
    """Строки таблицы как {name, description, schema, required}."""
    header = [clean_text(h).replace("*", "").strip().lower() for h in table["header"]]
    if not header or not header[0].startswith("параметр"):
        return []

    def col(*names):
        for n in names:
            for idx, h in enumerate(header):
                if h.startswith(n):
                    return idx
        return None

    c_desc = col("описание", "значение")
    c_vals = col("допустимые")
    c_type = col("тип", "формат")
    c_req = col("обязательн")
    out = []
    for row in table["rows"]:
        if len(row) < 2:
            continue
        # В одной ячейке может быть несколько имён, по одному на строку (scrollType, scrollId…).
        cell_names = [clean_name(x) for x in re.split(r"\n|<br\s*/?>", row[0]) if x.strip()]
        cell_names = [n for n in cell_names if NAME_RE.match(n)]
        if not cell_names:
            continue
        desc = clean_text(row[c_desc]) if c_desc is not None and c_desc < len(row) else ""
        if c_vals is not None and c_vals < len(row) and row[c_vals].strip():
            desc = (desc + "\nДопустимые значения: " + clean_text(row[c_vals])).strip()
        type_text = row[c_type] if c_type is not None and c_type < len(row) else ""
        schema = map_type(type_text)
        if not schema and clean_text(type_text):
            desc = (desc + "\nТип: " + clean_text(type_text)).strip()
        req = table["required"]
        if c_req is not None and c_req < len(row):
            req = clean_text(row[c_req]).lower().startswith("да")
        # Несколько имён в одной ячейке и столько же типов по строкам (scrollType, perScroll…):
        # тип у каждого имени свой.
        type_lines = [x for x in re.split(r"\n|<br\s*/?>", type_text) if x.strip()]
        per_name = len(cell_names) > 1 and len(type_lines) == len(cell_names)
        for idx, name in enumerate(cell_names):
            own = map_type(type_lines[idx]) if per_name else schema
            out.append({"name": name, "description": desc, "schema": own, "required": bool(req)})
    return out


def find_property(schema: dict, name: str) -> dict | None:
    """Ищет свойство name на любом уровне вложенности схемы; «a.b» — путь от найденного a."""
    if "." in name:
        head, rest = name.split(".", 1)
        node = find_property(schema, head)
        return find_property(node, rest) if node is not None else None
    props = schema.get("properties") or schema.get("items", {}).get("properties")
    if not props:
        return None
    if name in props:
        return props[name]
    for prop in props.values():
        found = find_property(prop, name)
        if found is not None:
            return found
    return None


def object_schema(block: str) -> dict | None:
    """Схема объекта из таблиц блока: верхний уровень и «Поля объекта `x`»."""
    root = {"type": "object", "properties": {}}
    required = []
    for table in parse_tables(block):
        rows = table_rows(table)
        if not rows:
            continue
        if table["target"]:
            prop = find_property(root, table["target"])
            if prop is None:
                continue
            holder = prop
            if prop.get("type") == "array":
                holder = prop.setdefault("items", {})
                holder.setdefault("type", "object")
            elif "anyOf" in prop:
                holder = prop["anyOf"][0]
            elif "type" not in prop:
                # «объект или строка»: описываем вариант-объект
                holder = {"type": "object"}
                prop["anyOf"] = [holder, {"type": "string"}, {"type": "number"}]
            elif prop["type"] != "object":
                # В таблице тип «Строка», а ниже описаны поля объекта (board при создании спринта):
                # принимаем и объект, и прежний тип.
                holder = {"type": "object"}
                prop["anyOf"] = [holder, {"type": prop.pop("type")}]
            props = holder.setdefault("properties", {})
            for r in rows:
                if PLACEHOLDER_RE.match(r["name"]):
                    holder["additionalProperties"] = prop_schema(r)
                    continue
                if r["name"] not in props:
                    props[r["name"]] = prop_schema(r)
                    if table["required"] and r["required"]:
                        holder.setdefault("required", []).append(r["name"])
            continue
        for r in rows:
            if PLACEHOLDER_RE.match(r["name"]):
                # «КЛЮЧ» и подобное в справочнике означает любое поле, например ключ поля задачи.
                root["additionalProperties"] = prop_schema(r)
                continue
            if r["name"] not in root["properties"]:
                root["properties"][r["name"]] = prop_schema(r)
                if r["required"]:
                    required.append(r["name"])
    if not root["properties"] and "additionalProperties" not in root:
        return None
    if required:
        root["required"] = required
    return root


def prop_schema(row: dict) -> dict:
    schema = dict(row["schema"])
    if row["description"]:
        schema["description"] = row["description"]
    return schema


def parse_answer(text: str) -> dict:
    """Код успешного ответа, коды ошибок и пример ответа."""
    m = re.search(r"^## Формат ответа.*?$(.*?)(?=^## |\Z)", text, re.S | re.M)
    answer = m.group(1) if m else ""
    included = "\n".join(re.findall(r"\{% included \([^)]*\) %\}(.*?)(?=\{% included|\Z)", text, re.S))
    code = re.search(r"ответ с кодом `(\d{3})", answer)
    errors = {}
    for em in re.finditer(r"^\s*(\d{3})\s*\n\s*:\s+(.+)$", answer, re.M):
        errors[em.group(1)] = clean_text(em.group(2))
    example = None
    is_array = None
    for src in (answer, included):
        jm = re.search(r"```json[^\n]*\n(.*?)```", src, re.S)
        if jm:
            raw = jm.group(1).strip()
            is_array = raw.startswith("[")
            example = parse_example(raw)
            break
    return {"code": code.group(1) if code else None, "errors": errors, "example": example, "is_array": is_array}


def parse_example(raw: str):
    """Пример из справочника с маскированными значениями и «{...}» приводит к JSON, если получится."""
    fixed = re.sub(r",?\s*\{\s*\.\.\.\s*\}", "", raw)
    fixed = re.sub(r",?\s*\.\.\.\s*(?=[\]}])", "", fixed)
    fixed = re.sub(r'(?<=[:\[,\s])(\d+\*+[\d*]*)(?=\s*[,}\]\n])', r'"\1"', fixed)
    fixed = re.sub(r",(\s*[}\]])", r"\1", fixed)
    try:
        return json.loads(fixed)
    except ValueError:
        return None


def parse_page(name: str, text: str) -> dict | None:
    body = strip_front_matter(text)
    h1 = re.search(r"^# (.+?)\s*(\{#.*\})?\s*$", body, re.M)
    title = h1.group(1).strip() if h1 else name
    methods = [(m.upper(), u.strip()) for m, u in METHOD_BLOCK_RE.findall(body)]
    if not methods:
        methods = FALLBACK_METHOD_RE.findall(body)[:1]
    if not methods:
        return None
    intro = ""
    if h1:
        rest = body[h1.end():]
        para = []
        for line in rest.split("\n"):
            s = line.strip()
            if s.startswith("<div") or s.startswith("##") or s.startswith("{%"):
                if para:
                    break
                if s.startswith("<div") or s.startswith("##"):
                    break
                continue
            if not s and para:
                para.append("")
                continue
            if s:
                para.append(s)
        intro = clean_text("\n".join(para))
    secs = sections(body)
    return {
        "file": name,
        "title": title,
        "intro": intro,
        "methods": methods,
        "sections": secs,
        "answer": parse_answer(body),
        "response_block": response_block(body, secs),
        # Заголовки постраничной выдачи, которые страница упоминает в тексте.
        "headers": [
            h for h, pattern in (
                ("XTotalPages", r"X-Total-Pages"),
                ("XTotalCount", r"X-Total-Count"),
                ("Link", r"заголов\w*\s+(ответа\s+)?(\*\*|`)Link|\|\|?\s*Link\s*\|"),
            ) if re.search(pattern, body)
        ],
        "multipart": "multipart/form-data" in body,
        "if_match": if_match_text(secs),
    }


def if_match_text(secs: list[tuple[str, str]]) -> str | None:
    """Описание заголовка If-Match из блока «Заголовки», если страница его упоминает."""
    for title, block in secs:
        if title != "Заголовки":
            continue
        m = re.search(r"^\s*[*-]\s+`If-Match`:\s*(.+)$", block, re.M)
        if m:
            text = clean_text(m.group(1)).replace("`", "")
            return text[:1].upper() + text[1:]
    return None


def url_query_names(url: str) -> list[str]:
    """Имена параметров запроса в адресе метода: «…/{id}?version=VERSION» -> ["version"]."""
    query = url.split("?", 1)[1] if "?" in url else ""
    return re.findall(r"(?:^|&)([A-Za-z]\w*)=", query)


def response_block(body: str, secs: list[tuple[str, str]]) -> str:
    """Текст с таблицами параметров ответа.

    Обычно это блок {% cut "Параметры ответа" %}, но на части страниц таблицы стоят
    под заголовком «#### Параметры ответа» или в блоке «Параметры объекта …»
    внутри вкладки успешного ответа (связи, комментарий, метаданные вложения).
    """
    for title, block in secs:
        if title == "Параметры ответа":
            return block
    m = re.search(r"^## Формат ответа.*?$(.*?)(?=^## |\Z)", body, re.S | re.M)
    if not m:
        return ""
    success = re.split(r"^\s*- Запрос выполнен с ошибкой", m.group(1), maxsplit=1, flags=re.M)[0]
    head = re.search(r"^\s*#+ Параметры ответа.*$", success, re.M)
    if head:
        return success[head.end():]
    for title, block in sections(success):
        if title.startswith("Параметры") and "запроса" not in title:
            return block
    return ""


# ---------- сборка OpenAPI ----------

def normalize_path(url: str) -> tuple[str, dict[str, str]]:
    """Путь с единообразными именами параметров и соответствие «новое имя -> имя в справочнике»."""
    url = url.replace(API_SERVER, "").strip()
    url = re.sub(r"\?.*$", "", url).rstrip("/")
    segs = url.split("/")
    out, names, prev, prev_param = [], {}, None, None
    for seg in segs:
        m = re.fullmatch(r"\{([^}]+)\}", seg)
        is_placeholder = re.fullmatch(r"[A-Z][A-Z_]+", seg) is not None
        if m or is_placeholder:
            original = m.group(1) if m else seg
            if prev_param and prev_param in PATH_PARAM_AFTER_PARAM:
                new = PATH_PARAM_AFTER_PARAM[prev_param]
            else:
                new = PATH_PARAM_BY_SEGMENT.get(prev or "", re.sub(r"\W", "_", original))
            names[new] = original
            out.append("{" + new + "}")
            prev_param = new
        else:
            out.append(seg)
            prev, prev_param = seg, None
    return "/".join(out), names


def camel(slug: str) -> str:
    parts = re.split(r"[-_]", slug)
    return parts[0] + "".join(p[:1].upper() + p[1:] for p in parts[1:])


def access_level(method: str, path: str) -> str:
    for m, pattern in DESTRUCTIVE_PATTERNS:
        if method == m and pattern.search(path):
            return "destructive"
    if method == "GET":
        return "read"
    if method == "POST" and path.endswith(READ_ONLY_POST_SUFFIXES):
        return "read"
    if method == "DELETE":
        return "destructive"
    return "write"


def richness(op: dict) -> int:
    """Насколько подробно описана операция: число параметров и полей тела и ответа."""
    n = len([p for p in op.get("parameters", []) if "$ref" not in p])
    for media in op.get("requestBody", {}).get("content", {}).values():
        n += len(media["schema"].get("properties", {}))
    for resp in op.get("responses", {}).values():
        for media in resp.get("content", {}).values():
            sch = media.get("schema", {})
            n += len(sch.get("properties") or sch.get("items", {}).get("properties") or {})
    return n


def build(pages: dict[str, str]) -> tuple[dict, list[str]]:
    report = []
    paths: dict[str, dict] = {}
    op_ids: set[str] = set()
    parsed_count = 0
    for name in sorted(pages):
        if name in ("common-format.md",):
            continue
        page = parse_page(name, pages[name])
        if page is None:
            report.append(f"без метода: {name}")
            continue
        parsed_count += 1
        section = name.split("__")[0] if "__" in name else "macros"
        slug = name.split("__")[-1][:-3]
        sec = {}
        for title, block in page["sections"]:
            sec.setdefault(title, block)
        resource_rows = {}
        for t in parse_tables(sec.get("Ресурс", "")):
            for r in table_rows(t):
                resource_rows[r["name"]] = r
        query_rows = []
        for t in parse_tables(sec.get("Параметры запроса", "")):
            if t["target"]:
                continue
            query_rows.extend(table_rows(t))
        body_schema = object_schema(sec.get("Параметры тела запроса", ""))
        resp_schema = object_schema(page["response_block"])

        for idx, (method, url) in enumerate(page["methods"]):
            path, names = normalize_path(url)
            item = paths.setdefault(path, {})
            key = method.lower()
            op_id = camel(slug) + ("" if idx == 0 else f"{idx + 1}")
            if op_id in op_ids:
                op_id = camel(section) + op_id[:1].upper() + op_id[1:]
            op_ids.add(op_id)
            docs_url = DOCS_BASE + name.replace("__", "/")[:-3]
            op = {
                "operationId": op_id,
                "tags": [TAG_NAME.get(section, section)],
                "summary": page["title"],
            }
            if page["intro"]:
                op["description"] = page["intro"]
            op["externalDocs"] = {"url": docs_url}
            op["x-access-level"] = access_level(method, path)
            params = []
            url_query = url_query_names(url)
            unused = [
                r for k, r in resource_rows.items() if k not in names.values() and k not in names and k not in url_query
            ]
            for new, original in names.items():
                r = resource_rows.get(original) or resource_rows.get(new)
                if r is None:
                    # Иногда параметр пути описан в таблице «Параметры запроса».
                    r = next((q for q in query_rows if q["name"] in (original, new)), None)
                if r is None and unused:
                    # имя в адресе и в таблице «Ресурс» не совпало: берём следующую свободную строку
                    r = unused.pop(0)
                p = {"name": new, "in": "path", "required": True, "schema": {"type": "string"}}
                if r and r["description"]:
                    p["description"] = r["description"]
                elif new in DEFAULT_PATH_DESCRIPTIONS:
                    p["description"] = DEFAULT_PATH_DESCRIPTIONS[new]
                if new in PATH_PARAM_NOTES and PATH_PARAM_NOTES[new] not in p.get("description", ""):
                    p["description"] = (p.get("description", "") + "\n" + PATH_PARAM_NOTES[new]).strip()
                if original != new:
                    p["x-docs-name"] = original
                params.append(p)
                if "description" not in p:
                    report.append(f"нет описания параметра пути {original}: {method} {path}")
            seen = set(names) | set(names.values())
            for r in query_rows:
                if r["name"] in seen:
                    continue
                seen.add(r["name"])
                p = {"name": r["name"], "in": "query", "required": r["required"], "schema": r["schema"] or {"type": "string"}}
                p["schema"] = QUERY_PARAM_SCHEMAS.get((method, path), {}).get(r["name"], p["schema"])
                if r["description"]:
                    p["description"] = r["description"]
                params.append(p)
            # Параметр из адреса («…?version=VERSION»), описанный в таблице «Ресурс», а не
            # в «Параметрах запроса» (изменение рабочего процесса, триггера, компонента).
            for qname in url_query:
                r = resource_rows.get(qname)
                if qname in seen or r is None:
                    continue
                seen.add(qname)
                p = {"name": qname, "in": "query", "required": r["required"], "schema": r["schema"] or {"type": "string"}}
                p["schema"] = QUERY_PARAM_SCHEMAS.get((method, path), {}).get(qname, p["schema"])
                if r["description"]:
                    p["description"] = r["description"]
                params.append(p)
            version_fix = VERSION_QUERY_PARAMS.get((method, path), {})
            if "version" not in seen and (page["if_match"] or version_fix.get("note")):
                seen.add("version")
                desc = version_fix.get("note") or f"{page['if_match']}\n{VERSION_QUERY_NOTE}"
                p = {"name": "version", "in": "query", "required": False, "schema": {"type": "integer"}, "description": desc}
                params.append(p)
                report.append(f"версия в параметре запроса вместо If-Match: {method} {path}")
            if "required" in version_fix:
                for p in params:
                    if p["name"] == "version":
                        p["required"] = version_fix["required"]
            params += [{"$ref": "#/components/parameters/XOrgId"}, {"$ref": "#/components/parameters/XCloudOrgId"}]
            op["parameters"] = params

            if page["multipart"] and method == "POST":
                schema = {"type": "object", "properties": {"file": {"type": "string", "format": "binary", "description": "Файл."}}, "required": ["file"]}
                op["requestBody"] = {"required": True, "content": {"multipart/form-data": {"schema": schema}}}
            elif body_schema and method in ("POST", "PATCH", "PUT", "DELETE"):
                schema = apply_body_overrides(method, path, json.loads(json.dumps(body_schema)))
                required = bool(schema.get("required") or schema.get("minProperties"))
                op["requestBody"] = {"required": required, "content": {"application/json": {"schema": schema}}}
            elif method in ("POST", "PATCH", "PUT") and "Параметры тела запроса" in sec:
                # Таблицы нет: тело описано словами (например, «как при редактировании задачи»).
                text = clean_text(sec["Параметры тела запроса"])
                op["requestBody"] = {"required": False, "content": {"application/json": {"schema": {"type": "object", "description": text}}}}
                report.append(f"тело без таблицы: {method} {path} ({name})")

            ans = page["answer"]
            code = ans["code"] or ("201" if method == "POST" and access_level(method, path) == "write" else "200")
            responses = {}
            if code == "204":
                responses["204"] = {"description": "Запрос выполнен успешно, тело ответа пустое."}
            elif "/attachments/{fileId}/{fileName}" in path or "/thumbnails/" in path:
                responses[code] = {"description": "Содержимое файла.", "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}}}
            else:
                schema = json.loads(json.dumps(resp_schema or {}))
                # Примеры в справочнике иногда общие для списка и одного объекта, поэтому
                # массив ставим только там, где адрес не указывает на один объект.
                last = path.rsplit("/", 1)[-1]
                is_list = bool(ans["is_array"]) and not last.startswith("{") and method in ("GET", "POST") and code != "201"
                if is_list and schema:
                    schema = {"type": "array", "items": schema}
                elif is_list:
                    schema = {"type": "array", "items": {}}
                elif not schema and isinstance(ans["example"], dict):
                    # Таблицы параметров ответа нет, но пример — объект.
                    schema = {"type": "object"}
                schema = apply_response_overrides(method, path, schema)
                media = {"schema": schema}
                if ans["example"] is not None:
                    media["example"] = ans["example"]
                responses[code] = {"description": "Запрос выполнен успешно.", "content": {"application/json": media}}
                query_names = {p["name"] for p in params if p.get("in") == "query"}
                headers = list(page["headers"])
                if path == "/v3/issues/_search":
                    headers += [*PAGINATION_HEADERS, *SCROLL_HEADERS]
                elif {"page", "perPage"} <= query_names:
                    # Общий формат постраничной выдачи (страница справочника «Общий формат»).
                    headers += list(PAGINATION_HEADERS)
                if headers:
                    responses[code]["headers"] = {
                        HEADER_NAMES[h]: {"$ref": f"#/components/headers/{h}"} for h in dict.fromkeys(headers)
                    }
            for ecode, edesc in sorted(ans["errors"].items()):
                responses[ecode] = {"$ref": f"#/components/responses/Error{ecode}"} if ecode in COMMON_ERRORS else {"description": edesc}
            if not ans["errors"]:
                responses["default"] = {"$ref": "#/components/responses/Error"}
            op["responses"] = responses
            if key in item:
                # Один метод описан на нескольких страницах: основной берём более подробную,
                # вторую упоминаем в описании.
                main, other = (op, item[key]) if richness(op) > richness(item[key]) else (item[key], op)
                main["description"] = (
                    main.get("description", "")
                    + f"\n\nСм. также «{other['summary']}»: {other['externalDocs']['url']}\n{other.get('description', '')}"
                ).strip()
                report.append(f"объединено: {method} {path}: основная «{main['summary']}», дополнительная «{other['summary']}»")
                op = main
            item[key] = op

    spec = {
        "openapi": "3.1.0",
        "info": {
            "title": "Yandex Tracker API v3",
            "version": date.today().isoformat(),
            "summary": "Неофициальная спецификация, собранная из справочника API Яндекс Трекера.",
            # Страницы справочника из llms.txt, по которым собрана спецификация: по ним
            # проверка --check видит новые и удалённые страницы.
            "x-source-pages": sorted(page_name(name) for name in pages),
            "description": (
                "Собрано скриптом `crates/tracker-api/spec/build_openapi.py` из markdown-версий страниц справочника "
                "https://yandex.ru/support/tracker/ru/api/about-api. Официальной OpenAPI-спецификации у Трекера нет.\n\n"
                "Ограничения:\n"
                "- схемы ответов описывают основные поля из таблиц «Параметры ответа», "
                "в задачах могут быть и пользовательские поля;\n"
                "- параметры, тип которых в справочнике описан словами («объект, число или строка»), "
                "оставлены без строгого типа, исходное описание типа — в description;\n"
                "- имена параметров пути приведены к единому виду, исходное имя из справочника — в `x-docs-name`;\n"
                "- места, где справочник расходится с реальным API (форма ответов, обязательные поля, тело "
                "освобождения прокрутки), исправлены исключениями в скрипте сборки, список — в "
                "`crates/tracker-api/spec/README.md`.\n\n"
                "`x-access-level` — уровень доступа операции для MCP-сервера: `read`, `write` или `destructive`."
            ),
        },
        "externalDocs": {"description": "Справочник API Яндекс Трекера", "url": DOCS_BASE + "about-api"},
        "servers": [{"url": API_SERVER}],
        "security": [{"OAuth": []}, {"IamToken": []}],
        "tags": [{"name": n, "description": d} for _, n, d in TAGS],
        "paths": dict(sorted(paths.items())),
        "components": {
            "securitySchemes": {
                "OAuth": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "Authorization",
                    "description": "OAuth-токен в формате `OAuth <токен>`. Подходит для организаций Яндекс 360 и Yandex Cloud.",
                },
                "IamToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "IAM-токен (`Bearer <токен>`), только для организаций Yandex Cloud. Действует не больше 12 часов.",
                },
            },
            "parameters": {
                "XOrgId": {
                    "name": "X-Org-ID",
                    "in": "header",
                    "required": False,
                    "description": "Идентификатор организации Яндекс 360 для бизнеса. Нужен один из заголовков: X-Org-ID или X-Cloud-Org-ID.",
                    "schema": {"type": "string"},
                },
                "XCloudOrgId": {
                    "name": "X-Cloud-Org-ID",
                    "in": "header",
                    "required": False,
                    "description": "Идентификатор организации Yandex Identity Hub. Нужен один из заголовков: X-Org-ID или X-Cloud-Org-ID.",
                    "schema": {"type": "string"},
                },
            },
            "headers": RESPONSE_HEADERS,
            "responses": {
                **{f"Error{c}": {"description": d, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}} for c, d in COMMON_ERRORS.items()},
                "Error": {"description": "Ошибка.", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    # Поля по реальным ответам API: errorsData и invocation-info в справочнике не описаны.
                    "properties": {
                        "errors": {
                            "type": "object",
                            "description": "Ошибки по полям: имя поля или параметра -> текст ошибки.",
                            "additionalProperties": {"type": "string"},
                        },
                        "errorsData": {"type": "object", "description": "Дополнительные данные об ошибках по полям."},
                        "errorMessages": {"type": "array", "items": {"type": "string"}, "description": "Общие сообщения об ошибке."},
                        "statusCode": {"type": "integer", "description": "HTTP-код ответа."},
                        "invocation-info": {
                            "type": "object",
                            "description": "Служебные сведения о запросе, например параметры постраничной выдачи.",
                        },
                    },
                }
            },
        },
    }
    ops = sum(len(v) for v in paths.values())
    report.insert(0, f"страниц с методами: {parsed_count}, операций: {ops}, путей: {len(paths)}")
    return spec, report


# Описания параметров пути для страниц без таблицы «Ресурс».
DEFAULT_PATH_DESCRIPTIONS = {
    "issueId": "Идентификатор или ключ задачи.",
    "filterId": "Идентификатор фильтра.",
    "fileName": "Имя файла.",
    "runId": "Идентификатор запуска автодействия.",
}

COMMON_ERRORS = {
    "400": "Один или несколько параметров запроса имеют недопустимое значение или формат.",
    "401": "Пользователь не авторизован.",
    "403": "Не хватает прав на выполнение этого действия.",
    "404": "Запрошенный объект не найден.",
    "409": "Конфликт: объект изменён или уже существует.",
    "412": "Конфликт версий объекта.",
    "422": "Ошибка валидации JSON.",
    "428": "Нужно указать версию объекта.",
}


# ---------- проверка расхождений со справочником (--check) ----------

# Коды выхода --check: изменений нет, спецификация отстала, ошибка сети или разбора.
EXIT_OK, EXIT_ERROR, EXIT_DRIFT = 0, 1, 3


def spec_ops(spec: dict) -> dict[tuple[str, str], dict]:
    return {(m.upper(), path): op for path, item in spec.get("paths", {}).items() for m, op in item.items()}


def op_params(op: dict) -> dict[str, dict]:
    """Параметры и поля тела верхнего уровня: «query version» -> {required, schema, description}."""
    out = {}
    for p in op.get("parameters", []):
        if "$ref" in p:
            out["ref " + p["$ref"].rsplit("/", 1)[-1]] = {}
            continue
        out[f"{p['in']} {p['name']}"] = {k: p.get(k) for k in ("required", "schema", "description")}
    for media in op.get("requestBody", {}).get("content", {}).values():
        schema = media.get("schema", {})
        required = set(schema.get("required", []))
        for name, prop in schema.get("properties", {}).items():
            prop = dict(prop)
            out[f"body {name}"] = {
                "required": name in required,
                "description": prop.pop("description", None),
                "schema": prop,
            }
    return out


def param_brief(p: dict) -> str:
    schema = p.get("schema") or {}
    kind = schema.get("type") or ("anyOf" if "anyOf" in schema else "")
    parts = [x for x in (kind, "обязательный" if p.get("required") else "") if x]
    return f" ({', '.join(parts)})" if parts else ""


def param_changes(old: dict, new: dict) -> list[str]:
    out = []
    for key in sorted(set(old) | set(new)):
        a, b = old.get(key), new.get(key)
        if a is None:
            out.append(f"+ {key}{param_brief(b)}")
        elif b is None:
            out.append(f"- {key}{param_brief(a)}")
        elif a != b:
            what = []
            if bool(a.get("required")) != bool(b.get("required")):
                what.append("обязательный" if b.get("required") else "необязательный")
            if a.get("schema") != b.get("schema"):
                what.append("схема")
            if a.get("description") != b.get("description"):
                what.append("описание")
            out.append(f"~ {key}: {', '.join(what)}")
    return out


OP_FIELD_NAMES = {
    "summary": "заголовок",
    "description": "описание",
    "tags": "раздел",
    "externalDocs": "страница справочника",
    "x-access-level": "уровень доступа",
    "requestBody": "тело запроса",
    "responses": "ответы",
}


def drift(old: dict, new: dict) -> list[str]:
    """Разница между спецификацией в репозитории (old) и свежей сборкой (new); пусто — изменений нет."""
    lines: list[str] = []

    def section(title: str, items: list[str]) -> None:
        if items:
            lines.append(f"{title} ({len(items)}):")
            lines.extend(f"  {x}" for x in items)

    old_pages = set(old.get("info", {}).get("x-source-pages", []))
    new_pages = set(new.get("info", {}).get("x-source-pages", []))
    if old_pages:
        section("Новые страницы справочника", [f"+ {x}" for x in sorted(new_pages - old_pages)])
        section("Удалённые страницы справочника", [f"- {x}" for x in sorted(old_pages - new_pages)])
    else:
        lines.append("В спецификации нет списка страниц (info.x-source-pages): страницы не сравниваются.")

    old_ops, new_ops = spec_ops(old), spec_ops(new)

    def title(key: tuple[str, str], op: dict) -> str:
        return f"{key[0]} {key[1]} ({op.get('operationId')}, {op.get('x-access-level')})"

    section("Новые операции", [f"+ {title(k, new_ops[k])}" for k in sorted(new_ops.keys() - old_ops.keys())])
    section("Удалённые операции", [f"- {title(k, old_ops[k])}" for k in sorted(old_ops.keys() - new_ops.keys())])
    renamed, params, other = [], [], []
    for key in sorted(old_ops.keys() & new_ops.keys()):
        a, b = old_ops[key], new_ops[key]
        name = f"{key[0]} {key[1]} ({b.get('operationId')})"
        if a.get("operationId") != b.get("operationId"):
            renamed.append(f"~ {key[0]} {key[1]}: {a.get('operationId')} -> {b.get('operationId')}")
        changes = param_changes(op_params(a), op_params(b))
        if changes:
            params.append(f"~ {name}: " + "; ".join(changes))
        fields = []
        for field in sorted((set(a) | set(b)) - {"parameters", "operationId"}):
            if a.get(field) != b.get(field):
                label = OP_FIELD_NAMES.get(field, field)
                if field == "x-access-level":
                    label += f" {a.get(field)} -> {b.get(field)}"
                fields.append(label)
        if fields:
            other.append(f"~ {name}: {', '.join(fields)}")
    section("Изменён operationId (меняется имя инструмента)", renamed)
    section("Изменены параметры и поля тела", params)
    section("Изменено другое", other)

    def rest(spec: dict) -> dict:
        info = {k: v for k, v in spec.get("info", {}).items() if k not in ("version", "x-source-pages")}
        return {**{k: v for k, v in spec.items() if k != "paths"}, "info": info}

    a, b = rest(old), rest(new)
    section("Изменены разделы вне операций", sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k)))
    return lines


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cache", help="каталог для скачанных страниц (по умолчанию — временный)")
    ap.add_argument("--offline", action="store_true", help="не скачивать, взять страницы из кеша")
    ap.add_argument(
        "--check",
        action="store_true",
        help=(
            "не записывать --out, а сравнить с ним свежую сборку и напечатать разницу; "
            f"код выхода {EXIT_OK} — изменений нет, {EXIT_DRIFT} — есть, {EXIT_ERROR} — ошибка сети или разбора"
        ),
    )
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "tracker.openapi.json"))
    args = ap.parse_args()
    if args.offline and not args.cache:
        ap.error("для --offline нужен --cache")
    with tempfile.TemporaryDirectory(prefix="tracker-api-docs-") as tmp:
        cache = args.cache or tmp
        try:
            pages = load_pages(cache, args.offline)
            spec, report = build(pages)
        except (OSError, SourceError) as err:
            # urllib.error.URLError и тайм-ауты — подклассы OSError.
            print(f"ошибка загрузки справочника: {err}", file=sys.stderr)
            return EXIT_ERROR
        except Exception as err:
            # Сбой разбора (сайт поменял разметку) — ошибка, а не расхождение со справочником.
            print(f"ошибка разбора справочника: {type(err).__name__}: {err}", file=sys.stderr)
            return EXIT_ERROR
        for line in report:
            print(line, file=sys.stderr)
        if not args.check:
            with open(args.out, "w", encoding="utf-8") as fh:
                json.dump(spec, fh, ensure_ascii=False, indent=2)
                fh.write("\n")
            return EXIT_OK
        fresh = os.path.join(cache, "tracker.openapi.json")
        with open(fresh, "w", encoding="utf-8") as fh:
            json.dump(spec, fh, ensure_ascii=False, indent=2)
            fh.write("\n")
        try:
            with open(args.out, encoding="utf-8") as fh:
                current = json.load(fh)
        except (OSError, ValueError) as err:
            print(f"не удалось прочитать {args.out}: {err}", file=sys.stderr)
            return EXIT_ERROR
        lines = drift(current, spec)
        print(
            f"Справочник: страниц {len(pages)}, операций {len(spec_ops(spec))}; "
            f"в спецификации: страниц {len(current.get('info', {}).get('x-source-pages', []))}, "
            f"операций {len(spec_ops(current))}."
        )
        if not lines:
            print("Изменений нет: спецификация совпадает со справочником.")
            return EXIT_OK
        for line in lines:
            print(line)
        print("Спецификация отстала от справочника: пересоберите её скриптом и обновите обычным PR.")
        if args.cache:
            print(f"Свежая сборка: {fresh}")
        return EXIT_DRIFT


if __name__ == "__main__":
    sys.exit(main())
