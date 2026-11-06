//! Вырезание значений, похожих на учётные данные, из ответов API.
//!
//! В Tracker учётные данные встречаются в настройках интеграций: действия триггеров и
//! автодействий с HTTP-запросом (`Webhook`) хранят способ авторизации (`authContext`),
//! заголовки (`Authorization`, свои заголовки с ключами) и пароли или токены внешних сервисов,
//! а журнал вебхуков (`…/webhooks/log`) показывает заголовки отправленных запросов. Ещё токен
//! может оказаться в тексте задачи или комментария, если его туда вставили. Агенту эти
//! значения не нужны, а в его контексте они опасны: оттуда они легко попадают в логи, коммиты
//! и документацию. Поэтому во всех ответах — успешных и ошибках, JSON и текстовых — значения
//! заменяются на [`REDACTED`], а к результату добавляется пометка ([`Redactions::note`]).
//!
//! Правила:
//!
//! 1. **Поля по имени** ([`is_credential_key`]): строковое значение поля, имя которого похоже
//!    на пароль, токен, ключ или заголовок авторизации ([`CREDENTIAL_KEYS`]), на любой
//!    глубине. Исключения: идентификаторы постраничной выдачи (`scrollToken`, `pageToken`),
//!    признаки (`has_password`, `isSecret`) и пустые строки.
//! 2. **Способ авторизации** ([`is_auth_context_key`]): в объекте `authContext` (и
//!    `webhookAuthContext`) скрываются все строковые значения, кроме описательных
//!    ([`AUTH_CONTEXT_PLAIN`]: `type`, `login`, имя заголовка): формы объекта для разных
//!    способов авторизации в справочнике не описаны, поэтому на имена полей не полагаемся.
//! 3. **Заголовки интеграций**: на путях триггеров, автодействий, макросов и вебхуков
//!    ([`is_integration_path`]) в объекте `headers` скрываются значения всех заголовков, кроме
//!    заведомо безопасных ([`SAFE_HEADERS`]: `Content-Type`, `Date` и т. п.); так же — в
//!    списках пар `{"name", "value"}` и строк `Имя: значение`. Своё имя у заголовка с ключом
//!    может быть любым. На других путях `headers` — обычное поле (например, заголовки таблиц).
//! 4. **Значения по форме** ([`redact_secret_values`]) в любой строке: OAuth-токены Яндекса
//!    (`y0_…`, `AQAAAA…`), IAM-токены (`t1.…`), JWT, значения `OAuth …`, `Bearer …` и
//!    `Basic …`, блоки закрытых ключей PEM и пароли в URL (`https://user:пароль@host`).
//!    Заменяется только сам секрет, остальной текст остаётся. Значения полей постраничной
//!    выдачи не проверяются: они нужны агенту как есть.
//!
//! Для текстовых тел (не JSON) правило 1 применяется к парам `"ключ": "строка"` в тексте,
//! в том числе к незакрытой строке в конце (обрезанный JSON), правило 4 — ко всему тексту.
//!
//! Функции получают относительный путь запроса: от него зависит правило 3. Они не зависят
//! от `OperationDef`, поэтому работают и для сгенерированных инструментов, и для `api_request`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use percent_encoding::percent_decode_str;
use regex::{Captures, Regex};
use serde_json::{Map, Value};
use tracker_api::executor::{ApiError, ApiResponse, ResponseBody};

/// Чем заменяются скрытые значения.
pub const REDACTED: &str = "[redacted]";

/// Имена полей с учётными данными. Совпадение — имя целиком или суффикс после `_`
/// (`db_password`, `oauth_token`, `x_api_key`) без учёта регистра; camelCase, `-`, `.` и
/// пробелы приводятся к `_`.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "client_secret",
    "credentials",
    "authorization",
    "cookie",
];

/// Суффиксы имён, которые совпадают с [`CREDENTIAL_KEYS`], но не являются учётными данными:
/// идентификаторы постраничной выдачи нужны агенту для следующей страницы.
const NOT_CREDENTIAL_SUFFIXES: &[&str] = &["page_token", "scroll_token", "next_token"];

/// Поля постраничной выдачи: их значения не проверяются и по форме (правило 4).
const PAGINATION_SUFFIXES: &[&str] = &["page_token", "scroll_token", "next_token", "scroll_id"];

/// Префиксы флагов (`has_password`, `is_secret`): это признаки, а не значения.
const FLAG_PREFIXES: &[&str] = &["has_", "is_"];

/// Поля способа авторизации, которые описывают его, а не содержат секрет (правило 2).
/// Сравниваются с именем, приведённым к snake_case.
pub const AUTH_CONTEXT_PLAIN: &[&str] = &[
    "type",
    "auth_type",
    "scheme",
    "login",
    "user",
    "username",
    "user_name",
    "name",
    "header",
    "header_name",
    "token_type",
];

/// Заголовки, значения которых не скрываются в настройках и журнале интеграций (правило 3).
/// Сравниваются без учёта регистра.
pub const SAFE_HEADERS: &[&str] = &[
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
    "access-control-allow-origin",
    "access-control-expose-headers",
    "access-control-max-age",
    "cache-control",
    "connection",
    "content-disposition",
    "content-encoding",
    "content-language",
    "content-length",
    "content-type",
    "date",
    "etag",
    "expires",
    "host",
    "keep-alive",
    "last-modified",
    "pragma",
    "retry-after",
    "server",
    "strict-transport-security",
    "transfer-encoding",
    "user-agent",
    "vary",
    "x-content-type-options",
    "x-frame-options",
    "x-request-id",
];

/// Сегменты пути, после которых действует правило 3.
const INTEGRATION_SEGMENTS: &[&str] = &["triggers", "autoactions", "macros", "webhooks"];

/// Сколько раз раскодировать сегмент пути (как в проверке путей исполнителя).
const MAX_DECODE_ROUNDS: usize = 3;

/// Что было скрыто в одном ответе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// Имена полей, значения которых скрыты целиком (правила 1–3).
    pub credential_fields: BTreeSet<String>,
    /// Сколько значений, похожих на секреты по форме, скрыто внутри строк (правило 4).
    pub secret_values: usize,
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.credential_fields.is_empty() && self.secret_values == 0
    }

    fn merge(&mut self, other: Redactions) {
        self.credential_fields.extend(other.credential_fields);
        self.secret_values += other.secret_values;
    }

    /// Пояснение для агента; `None`, если ничего не скрыто.
    pub fn note(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.credential_fields.is_empty() {
            let fields: Vec<&str> = self.credential_fields.iter().map(String::as_str).collect();
            parts.push(format!(
                "Values of fields that look like credentials were replaced with \"{REDACTED}\": \
                 {}.",
                fields.join(", ")
            ));
        }
        if self.secret_values > 0 {
            parts.push(format!(
                "{} value(s) that look like tokens, private keys or passwords (OAuth or IAM \
                 tokens, Authorization values, passwords in URLs) were replaced with \
                 \"{REDACTED}\" inside the text.",
                self.secret_values
            ));
        }
        Some(format!(
            "[{} The server never shows them; do not ask the user for them, and do not try to \
             recover them in other ways.]",
            parts.join(" ")
        ))
    }
}

/// Похоже ли имя поля на поле с учётными данными (см. [`CREDENTIAL_KEYS`]).
pub fn is_credential_key(key: &str) -> bool {
    let key = normalize_key(key);
    if FLAG_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || NOT_CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
    {
        return false;
    }
    CREDENTIAL_KEYS.iter().any(|candidate| {
        key == *candidate
            || key
                .strip_suffix(candidate)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

/// Объект способа авторизации: `authContext`, `webhookAuthContext`, `auth_context`.
pub fn is_auth_context_key(key: &str) -> bool {
    let key = normalize_key(key);
    key == "auth_context" || key.ends_with("_auth_context")
}

fn is_headers_key(key: &str) -> bool {
    let key = normalize_key(key);
    key == "headers" || key.ends_with("_headers")
}

fn is_safe_header(name: &str) -> bool {
    let name = name.trim();
    SAFE_HEADERS
        .iter()
        .any(|safe| safe.eq_ignore_ascii_case(name))
}

fn is_pagination_key(key: &str) -> bool {
    let key = normalize_key(key);
    PAGINATION_SUFFIXES
        .iter()
        .any(|suffix| key == suffix.trim_start_matches('_') || key.ends_with(suffix))
}

/// Относится ли путь к настройкам или журналу интеграций (триггеры, автодействия, макросы,
/// вебхуки). Сегменты раскодируются и сравниваются без учёта регистра.
pub fn is_integration_path(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    path.split('/').map(decode_segment).any(|segment| {
        INTEGRATION_SEGMENTS
            .iter()
            .any(|known| segment.eq_ignore_ascii_case(known))
    })
}

fn decode_segment(segment: &str) -> String {
    let mut current = segment.to_owned();
    for _ in 0..MAX_DECODE_ROUNDS {
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            break;
        }
        current = decoded;
    }
    current.trim().to_owned()
}

/// `accessToken`, `Access-Token` → `access_token`.
fn normalize_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for c in key.trim().chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            normalized.push('_');
        }
        if matches!(c, '-' | ' ' | '.') {
            normalized.push('_');
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized
}

/// Где находится значение: от этого зависят правила 2 и 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Normal,
    /// Внутри `authContext`.
    AuthContext,
    /// Внутри `headers` на пути интеграции.
    Headers,
}

/// Вырезает значения из JSON-ответа запроса по пути `path`.
pub fn redact_json(path: &str, json: &mut Value) -> Redactions {
    let mut redactions = Redactions::default();
    let headers = is_integration_path(path);
    walk(json, Scope::Normal, headers, &mut redactions);
    redactions
}

fn walk(value: &mut Value, scope: Scope, headers: bool, redactions: &mut Redactions) {
    match value {
        Value::Object(map) => {
            if scope == Scope::Headers && redact_header_pair(map, redactions) {
                return;
            }
            for (key, item) in map.iter_mut() {
                if let Value::String(text) = item {
                    redact_field(key, text, scope, redactions);
                    continue;
                }
                let child = if is_auth_context_key(key) || scope == Scope::AuthContext {
                    Scope::AuthContext
                } else if headers && is_headers_key(key) {
                    Scope::Headers
                } else {
                    Scope::Normal
                };
                walk(item, child, headers, redactions);
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Value::String(text) = item {
                    if scope == Scope::Headers {
                        redact_header_line(text, redactions);
                    } else {
                        redactions.secret_values += redact_secret_values_in_place(text);
                    }
                    continue;
                }
                walk(item, scope, headers, redactions);
            }
        }
        Value::String(text) => {
            redactions.secret_values += redact_secret_values_in_place(text);
        }
        _ => {}
    }
}

/// Строковое значение поля `key` в области `scope`.
fn redact_field(key: &str, text: &mut String, scope: Scope, redactions: &mut Redactions) {
    if text.is_empty() {
        return;
    }
    let hide = is_credential_key(key)
        || match scope {
            Scope::Normal => false,
            Scope::AuthContext => !AUTH_CONTEXT_PLAIN.contains(&normalize_key(key).as_str()),
            Scope::Headers => !is_safe_header(key),
        };
    if hide {
        if text != REDACTED {
            *text = REDACTED.to_owned();
            redactions.credential_fields.insert(key.to_owned());
        }
        return;
    }
    if !is_pagination_key(key) {
        redactions.secret_values += redact_secret_values_in_place(text);
    }
}

/// Заголовок в виде `{"name": "X-Token", "value": "…"}` (или `key`/`header` вместо `name`).
/// Возвращает `true`, если объект — такая пара и обработан.
fn redact_header_pair(map: &mut Map<String, Value>, redactions: &mut Redactions) -> bool {
    let name = ["name", "key", "header"]
        .iter()
        .find_map(|field| map.get(*field).and_then(Value::as_str))
        .map(str::to_owned);
    let Some(name) = name else {
        return false;
    };
    let Some(Value::String(value)) = map.get_mut("value") else {
        return false;
    };
    if !value.is_empty() && value != REDACTED && !is_safe_header(&name) {
        *value = REDACTED.to_owned();
        redactions.credential_fields.insert(name);
    }
    true
}

/// Заголовок строкой `Имя: значение`.
fn redact_header_line(text: &mut String, redactions: &mut Redactions) {
    match text.split_once(':') {
        Some((name, value)) if !value.trim().is_empty() && !is_safe_header(name) => {
            if value.trim() != REDACTED {
                redactions.credential_fields.insert(name.trim().to_owned());
                *text = format!("{}: {REDACTED}", name.trim_end());
            }
        }
        Some(_) => {}
        None => redactions.secret_values += redact_secret_values_in_place(text),
    }
}

/// Значения по форме (правило 4): регулярные выражения и проверка найденного значения.
struct SecretPattern {
    regex: Regex,
    /// Группа с секретом; остальной текст совпадения сохраняется.
    group: usize,
    /// Дополнительная проверка секрета: отсекает похожие обычные слова.
    accept: fn(&str) -> bool,
}

fn any_value(_: &str) -> bool {
    true
}

/// Значение после `OAuth`/`Bearer`: токены содержат цифры, обычные слова — нет.
fn has_digit(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_digit())
}

/// Значение после `Basic`: base64 (длина кратна 4) с цифрой или выравниванием `=`.
fn looks_like_base64(value: &str) -> bool {
    value.len().is_multiple_of(4) && (value.ends_with('=') || has_digit(value))
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    let pattern = |source: &str, group: usize, accept: fn(&str) -> bool| {
        Regex::new(source).ok().map(|regex| SecretPattern {
            regex,
            group,
            accept,
        })
    };
    [
        // Блок закрытого ключа PEM (до конца блока или до конца текста, если он обрезан).
        pattern(
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            0,
            any_value,
        ),
        // Значение заголовка авторизации: схема остаётся, токен скрывается.
        pattern(
            r"\b(?:OAuth|Bearer)\s+([A-Za-z0-9._~+/=-][A-Za-z0-9._~+/=_-]{15,})",
            1,
            has_digit,
        ),
        pattern(r"\bBasic\s+([A-Za-z0-9+/]{8,}={0,2})", 1, looks_like_base64),
        // OAuth-токены Яндекса: новый формат `y0_…` и старый `AQAAAA…`.
        pattern(r"\by[0-9]_[A-Za-z0-9_-]{30,}", 0, any_value),
        pattern(r"\bAQAAAA[A-Za-z0-9_-]{25,}", 0, any_value),
        // IAM-токены Yandex Cloud.
        pattern(r"\bt1\.[A-Za-z0-9_-]+\.[A-Za-z0-9_=-]{40,}", 0, any_value),
        // JWT.
        pattern(
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any_value,
        ),
        // Пароль в URL: `scheme://user:пароль@host`.
        pattern(
            r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@"'<>]+:([^\s/@"'<>]+)@"#,
            1,
            any_value,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// Заменяет в тексте значения, похожие на секреты по форме (правило 4). Возвращает текст
/// и число замен.
pub fn redact_secret_values(text: &str) -> (String, usize) {
    let mut current = text.to_owned();
    let count = redact_secret_values_in_place(&mut current);
    (current, count)
}

fn redact_secret_values_in_place(text: &mut String) -> usize {
    // Быстрый выход для обычных строк: у всех шаблонов есть одна из этих подстрок.
    const MARKERS: &[&str] = &[
        "PRIVATE KEY",
        "OAuth",
        "Bearer",
        "Basic",
        "_",
        "AQAAAA",
        "t1.",
        "eyJ",
        "://",
    ];
    if !MARKERS.iter().any(|marker| text.contains(marker)) {
        return 0;
    }
    let mut count = 0;
    for pattern in SECRET_PATTERNS.iter() {
        let replaced = pattern.regex.replace_all(text, |caps: &Captures<'_>| {
            let whole = caps.get(0).map_or("", |m| m.as_str());
            let Some(secret) = caps.get(pattern.group) else {
                return whole.to_owned();
            };
            if secret.as_str() == REDACTED || !(pattern.accept)(secret.as_str()) {
                return whole.to_owned();
            }
            count += 1;
            let start = secret.start() - caps.get(0).map_or(0, |m| m.start());
            let end = start + secret.len();
            format!("{}{REDACTED}{}", &whole[..start], &whole[end..])
        });
        if let std::borrow::Cow::Owned(updated) = replaced {
            *text = updated;
        }
    }
    count
}

/// Вырезает значения из текстового тела: строковые значения пар `"ключ": "строка"` с ключами
/// учётных данных, в том числе незакрытая строка в конце (обрезанный JSON), и значения,
/// похожие на секреты по форме.
pub fn redact_text(_path: &str, text: &str) -> (String, Redactions) {
    let mut redactions = Redactions::default();
    let mut text = redact_credential_pairs(text, &mut redactions);
    redactions.secret_values += redact_secret_values_in_place(&mut text);
    (text, redactions)
}

fn redact_credential_pairs(text: &str, redactions: &mut Redactions) -> String {
    let bytes = text.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(offset) = text.get(i..).and_then(|rest| rest.find('"')) {
        let start = i + offset;
        let (key_end, after_key) = string_end(bytes, start);
        i = after_key;
        let colon = skip_ws(after_key);
        if bytes.get(colon) != Some(&b':') {
            continue;
        }
        let value_start = skip_ws(colon + 1);
        if bytes.get(value_start) != Some(&b'"') {
            continue;
        }
        let (value_end, after_value) = string_end(bytes, value_start);
        i = after_value;
        let key = text.get(start + 1..key_end).unwrap_or_default();
        let value = text.get(value_start + 1..value_end).unwrap_or_default();
        if !value.is_empty() && value != REDACTED && is_credential_key(key) {
            result.push_str(text.get(copied..value_start).unwrap_or_default());
            result.push('"');
            result.push_str(REDACTED);
            result.push('"');
            copied = after_value;
            redactions.credential_fields.insert(key.to_owned());
        }
    }
    result.push_str(text.get(copied..).unwrap_or_default());
    result
}

/// Для строки, открытой кавычкой в `start`: позиция закрывающей кавычки (или конец текста)
/// и позиция сразу после строки. Обе позиции — границы символов UTF-8: это `"` или конец.
fn string_end(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut j = start + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return (j, j + 1),
            _ => j += 1,
        }
    }
    (bytes.len(), bytes.len())
}

/// Вырезает значения из успешного ответа API и возвращает, что скрыто.
pub fn redact_response(path: &str, response: &mut ApiResponse) -> Redactions {
    match &mut response.body {
        ResponseBody::Json(json) => redact_json(path, json),
        ResponseBody::Text(text) => {
            let (redacted, redactions) = redact_text(path, text);
            *text = redacted;
            redactions
        }
        // Файл агент не видит, пустое тело скрывать нечего.
        ResponseBody::SavedToFile { .. } | ResponseBody::Empty => Redactions::default(),
    }
}

/// Ошибка API с вырезанными значениями в теле и сообщениях Tracker, и что скрыто.
pub fn redact_api_error(path: &str, err: ApiError) -> (ApiError, Redactions) {
    let ApiError::Http(mut http) = err else {
        return (err, Redactions::default());
    };
    let mut redactions = Redactions::default();
    http.body = match serde_json::from_str::<Value>(&http.body) {
        Ok(mut json) => {
            let found = redact_json(path, &mut json);
            if found.is_empty() {
                http.body
            } else {
                redactions.merge(found);
                json.to_string()
            }
        }
        Err(_) => {
            let (text, found) = redact_text(path, &http.body);
            redactions.merge(found);
            text
        }
    };
    http.error_messages = http
        .error_messages
        .into_iter()
        .map(|message| {
            let (text, found) = redact_text(path, &message);
            redactions.merge(found);
            text
        })
        .collect();
    http.errors = http
        .errors
        .into_iter()
        .map(|(field, message)| {
            let message = if is_credential_key(&field) && !message.is_empty() {
                redactions.credential_fields.insert(field.clone());
                REDACTED.to_owned()
            } else {
                let (text, found) = redact_text(path, &message);
                redactions.merge(found);
                text
            };
            (field, message)
        })
        .collect();
    (ApiError::Http(http), redactions)
}

/// Текст сообщения для агента (ошибки, не связанные с телом ответа): значения по форме и
/// пары `"ключ": "строка"`. Возвращает текст с пометкой, если что-то скрыто.
pub fn redact_message(text: &str) -> String {
    let (mut text, redactions) = redact_text("", text);
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use tracker_api::executor::{HttpError, Pagination};

    use super::*;

    const TRIGGERS: &str = "/v3/queues/TEST/triggers";
    const ISSUE: &str = "/v3/issues/TEST-1";
    /// Значения, похожие на настоящие токены по форме (не настоящие).
    const OAUTH: &str = "y0_AgAAAAAexampleexampleexampleexampleexampleexample";
    const IAM: &str = "t1.9euelZqexample.exampleexampleexampleexampleexampleexample12";
    /// Заголовок, данные и подпись JWT по отдельности.
    const JWT_PARTS: [&str; 3] = [
        "eyJhbGciOiJIUzI1NiJ9",
        "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
        "c2lnbmF0dXJlLXZhbHVl",
    ];
    const PEM_DASHES: &str = "-----";

    #[test]
    fn credential_keys() {
        for key in [
            "password",
            "Password",
            "db_pwd",
            "token",
            "accessToken",
            "oauth_token",
            "client-secret",
            "Authorization",
            "Proxy-Authorization",
            "privateKey",
            "apiKey",
            "apikey",
            "X-Api-Key",
            "X-Auth-Token",
            "Set-Cookie",
            "credentials",
        ] {
            assert!(is_credential_key(key), "{key}");
        }
        for key in [
            "key",
            "keys",
            "passportUid",
            "type",
            "has_password",
            "isSecret",
            "next_page_token",
            "nextToken",
            "scrollToken",
            "X-Scroll-Token",
            "tokens_count",
            "token_type",
            "author",
            "tokenizer",
        ] {
            assert!(!is_credential_key(key), "{key}");
        }
        assert!(is_auth_context_key("authContext"));
        assert!(is_auth_context_key("webhookAuthContext"));
        assert!(!is_auth_context_key("context"));
    }

    #[test]
    fn integration_paths() {
        for path in [
            TRIGGERS,
            "/v3/queues/TEST/triggers/5/webhooks/log",
            "/v3/queues/TEST/autoactions/2",
            "/v3/queues/TEST/macros",
            "/v3/queues/TEST/%74riggers",
        ] {
            assert!(is_integration_path(path), "{path}");
        }
        for path in [ISSUE, "/v3/dashboards/1/widgets", "/v3/issues/_search"] {
            assert!(!is_integration_path(path), "{path}");
        }
    }

    #[test]
    fn redacts_webhook_credentials_in_json() {
        let mut body = json!([{
            "id": 1,
            "actions": [{
                "type": "Webhook",
                "endpoint": "https://example.test/hook",
                "authContext": {
                    "type": "header",
                    "login": "bot",
                    "password": "p@ss",
                    "headerName": "X-Hook",
                    "headerValue": "hook-secret-1",
                    "nested": {"anything": "inner-secret"}
                },
                "headers": {"Authorization": "Bearer abc", "X-Signature": "sig-1", "Content-Type": "application/json"},
                "token": ""
            }, {
                "type": "Webhook",
                "headers": [
                    {"name": "X-Custom-Key", "value": "custom-1"},
                    {"name": "Accept", "value": "*/*"}
                ]
            }]
        }]);
        let redactions = redact_json(TRIGGERS, &mut body);
        let action = &body[0]["actions"][0];
        assert_eq!(action["authContext"]["password"], REDACTED);
        assert_eq!(action["authContext"]["headerValue"], REDACTED);
        assert_eq!(action["authContext"]["nested"]["anything"], REDACTED);
        assert_eq!(action["authContext"]["login"], "bot");
        assert_eq!(action["authContext"]["type"], "header");
        assert_eq!(action["authContext"]["headerName"], "X-Hook");
        assert_eq!(action["headers"]["Authorization"], REDACTED);
        assert_eq!(action["headers"]["X-Signature"], REDACTED);
        assert_eq!(action["headers"]["Content-Type"], "application/json");
        assert_eq!(action["endpoint"], "https://example.test/hook");
        // Пустое значение оставляем: скрывать нечего.
        assert_eq!(action["token"], "");
        let pairs = &body[0]["actions"][1]["headers"];
        assert_eq!(pairs[0]["value"], REDACTED);
        assert_eq!(pairs[0]["name"], "X-Custom-Key");
        assert_eq!(pairs[1]["value"], "*/*");
        assert_eq!(
            redactions.credential_fields.iter().collect::<Vec<_>>(),
            [
                "Authorization",
                "X-Custom-Key",
                "X-Signature",
                "anything",
                "headerValue",
                "password"
            ]
        );
        let note = redactions.note().unwrap();
        assert!(note.contains("Authorization, X-Custom-Key"), "{note}");
        assert!(Redactions::default().note().is_none());
    }

    #[test]
    fn webhook_log_headers_are_hidden() {
        let mut body = json!([{
            "request": {
                "headers": {"X-Startrek-Transport": "vNCc-1", "Content-Type": "application/json"},
                "webhookAuthContext": {"type": "noauth"}
            },
            "response": {"headers": {"Server": "nginx", "Set-Cookie": "sid=1"}}
        }]);
        redact_json("/v3/queues/TEST/triggers/5/webhooks/log", &mut body);
        assert_eq!(
            body[0]["request"]["headers"]["X-Startrek-Transport"],
            REDACTED
        );
        assert_eq!(
            body[0]["request"]["headers"]["Content-Type"],
            "application/json"
        );
        assert_eq!(body[0]["request"]["webhookAuthContext"]["type"], "noauth");
        assert_eq!(body[0]["response"]["headers"]["Server"], "nginx");
        assert_eq!(body[0]["response"]["headers"]["Set-Cookie"], REDACTED);
    }

    #[test]
    fn headers_elsewhere_are_ordinary_fields() {
        let mut body = json!({"headers": {"Name": "Задача", "Status": "Открыта"}});
        let redactions = redact_json("/v3/dashboards/1/widgets", &mut body);
        assert!(redactions.is_empty());
        assert_eq!(body["headers"]["Name"], "Задача");
    }

    #[test]
    fn secret_values_by_shape() {
        let cases = [
            (
                format!("token {OAUTH} in text"),
                "token [redacted] in text".to_owned(),
            ),
            (
                format!("Authorization: OAuth {OAUTH}"),
                "Authorization: OAuth [redacted]".to_owned(),
            ),
            (format!("Bearer {IAM}"), "Bearer [redacted]".to_owned()),
            (format!("iam={IAM};"), "iam=[redacted];".to_owned()),
            (
                "Basic Ym90OnBAc3M=".to_owned(),
                "Basic [redacted]".to_owned(),
            ),
            (
                "AQAAAAAexampleexampleexampleexample1".to_owned(),
                "[redacted]".to_owned(),
            ),
            (
                // Части JWT и ключа PEM собираются при выполнении: целиком в исходниках они
                // выглядели бы для сканеров секретов как настоящие.
                format!("jwt {}.{}.{}", JWT_PARTS[0], JWT_PARTS[1], JWT_PARTS[2]),
                "jwt [redacted]".to_owned(),
            ),
            (
                "clone https://bot:hunter2@git.example.test/repo.git".to_owned(),
                "clone https://bot:[redacted]@git.example.test/repo.git".to_owned(),
            ),
            (
                format!(
                    "key:\n{d}BEGIN OPENSSH PRIVATE KEY{d}\nAAAA\n{d}END OPENSSH PRIVATE KEY{d}\nend",
                    d = PEM_DASHES
                ),
                "key:\n[redacted]\nend".to_owned(),
            ),
            (
                format!(
                    "{d}BEGIN RSA PRIVATE KEY{d}\nMIIE (обрезано",
                    d = PEM_DASHES
                ),
                "[redacted]".to_owned(),
            ),
        ];
        for (input, expected) in cases {
            let (text, count) = redact_secret_values(&input);
            assert_eq!(text, expected, "{input}");
            assert_eq!(count, 1, "{input}");
        }
        // Обычный текст не меняется.
        for plain in [
            "Basic authentication is used",
            "Bearer tokens are described in the docs",
            "OAuth application for the user",
            "https://example.test/path?x=1",
            "mailto:user@example.test",
            "issue_key_value TEST-1",
            "y0_short",
            "dGVzdA==",
            "[redacted]",
        ] {
            assert_eq!(
                redact_secret_values(plain),
                (plain.to_owned(), 0),
                "{plain}"
            );
        }
    }

    #[test]
    fn secret_values_in_issue_text_and_pagination_ids() {
        let mut body = json!({
            "key": "TEST-1",
            "description": format!("Use `OAuth {OAUTH}` for the bot"),
            "comments": [format!("iam {IAM}")],
            "scrollId": OAUTH,
            "nextPageToken": OAUTH
        });
        let redactions = redact_json(ISSUE, &mut body);
        assert_eq!(body["description"], "Use `OAuth [redacted]` for the bot");
        assert_eq!(body["comments"][0], "iam [redacted]");
        assert_eq!(body["key"], "TEST-1");
        // Идентификаторы постраничной выдачи остаются как есть.
        assert_eq!(body["scrollId"], OAUTH);
        assert_eq!(body["nextPageToken"], OAUTH);
        assert_eq!(redactions.secret_values, 2);
        assert!(redactions.credential_fields.is_empty());
        let note = redactions.note().unwrap();
        assert!(note.contains("2 value(s)"), "{note}");
    }

    #[test]
    fn redacts_text_and_truncated_json() {
        let (text, redactions) = redact_text(
            TRIGGERS,
            r#"{"a": "b", "password" : "x\"y", "token": "tail"#,
        );
        assert_eq!(
            text,
            r#"{"a": "b", "password" : "[redacted]", "token": "[redacted]""#
        );
        assert_eq!(redactions.credential_fields.len(), 2);
        let (text, redactions) = redact_text(ISSUE, &format!("<html>{OAUTH}</html>"));
        assert_eq!(text, "<html>[redacted]</html>");
        assert_eq!(redactions.secret_values, 1);
        let (plain, redactions) = redact_text(TRIGGERS, "плain text without pairs");
        assert_eq!(plain, "плain text without pairs");
        assert!(redactions.is_empty());
    }

    #[test]
    fn redacts_responses_and_errors() {
        let mut response = ApiResponse {
            status: 200,
            body: ResponseBody::Json(json!({"secret": "s"})),
            pagination: Pagination::default(),
        };
        assert!(!redact_response(TRIGGERS, &mut response).is_empty());
        assert_eq!(
            response.body,
            ResponseBody::Json(json!({"secret": REDACTED}))
        );

        let err = ApiError::Http(Box::new(HttpError {
            status: 400,
            error_messages: vec![format!(r#"bad "password": "p1", header OAuth {OAUTH}"#)],
            errors: BTreeMap::from([
                ("token".to_owned(), "t1".to_owned()),
                ("summary".to_owned(), format!("got {IAM}")),
            ]),
            body: r#"{"errors":{"token":"t1"},"password":"p1"}"#.into(),
            retry_after: None,
        }));
        let (ApiError::Http(http), redactions) = redact_api_error(TRIGGERS, err) else {
            panic!("ожидалась ошибка HTTP");
        };
        assert!(
            !http.body.contains("p1") && !http.body.contains("t1"),
            "{}",
            http.body
        );
        assert!(!http.error_messages[0].contains("p1"));
        assert!(!http.error_messages[0].contains(OAUTH));
        assert_eq!(http.errors["token"], REDACTED);
        assert_eq!(http.errors["summary"], "got [redacted]");
        assert!(redactions.credential_fields.contains("password"));
        assert!(redactions.credential_fields.contains("token"));
        assert_eq!(redactions.secret_values, 2);

        // Тело без секретов возвращается как есть (без переформатирования).
        let err = ApiError::Http(Box::new(HttpError {
            status: 404,
            error_messages: vec!["Задача не существует.".into()],
            errors: BTreeMap::new(),
            body: r#"{ "errorMessages": ["Задача не существует."] }"#.into(),
            retry_after: None,
        }));
        let (ApiError::Http(http), redactions) = redact_api_error(ISSUE, err) else {
            panic!("ожидалась ошибка HTTP");
        };
        assert_eq!(
            http.body,
            r#"{ "errorMessages": ["Задача не существует."] }"#
        );
        assert!(redactions.is_empty());

        let (err, redactions) = redact_api_error(TRIGGERS, ApiError::MissingOrg);
        assert!(matches!(err, ApiError::MissingOrg));
        assert!(redactions.is_empty());
    }

    #[test]
    fn messages_get_a_note() {
        let text = redact_message(&format!(
            "could not reach https://bot:pw1@example.test ({OAUTH})"
        ));
        assert!(!text.contains("pw1") && !text.contains(OAUTH), "{text}");
        assert!(text.contains("2 value(s)"), "{text}");
        assert_eq!(redact_message("plain error"), "plain error");
    }
}
