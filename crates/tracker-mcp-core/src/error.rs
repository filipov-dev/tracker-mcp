//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. Токен в тексты ошибок не попадает: [`ApiError`]
//! его не содержит, аргументы инструмента токена не принимают, а сервер дополнительно
//! вырезает токен клиента из итогового текста.

use std::fmt;

use tracker_api::executor::{ApiError, HttpError};

/// Ошибка вызова инструмента с текстом для агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    fn from(err: ApiError) -> Self {
        Self(describe_api_error(&err))
    }
}

/// Текст ошибки исполнителя запросов для агента.
///
/// Тексты `ApiError` написаны для логов на русском, поэтому здесь своя формулировка на
/// английском; подробности от Tracker (`errorMessages`, `errors`, тело ответа) добавляются
/// как есть.
pub fn describe_api_error(err: &ApiError) -> String {
    match err {
        ApiError::Http(http) => describe_http_error(http),
        ApiError::MissingPathParam(name) => format!("missing path argument `{name}`"),
        ApiError::InvalidPath(_) => "an identifier argument does not form a valid API path: \
             empty values and values like `.` or `..` are rejected"
            .to_owned(),
        ApiError::InvalidRequest(detail) => format!("invalid request: {detail}"),
        ApiError::InvalidConfig(detail) => {
            // Подробности — для лога: они на русском и относятся к настройке сервера.
            tracing::warn!(%detail, "некорректная конфигурация клиента Tracker API");
            "the server is misconfigured (invalid token, organization or API address); ask \
             the user to check the server log and the profile settings"
                .to_owned()
        }
        ApiError::MissingToken => "no Tracker token is available for this request".to_owned(),
        ApiError::MissingOrg => "no Tracker organization is configured for this server; ask \
             the user to set it with `tracker-mcp auth login --org-id <ID>` (Yandex 360) or \
             `--cloud-org-id <ID>` (Yandex Cloud)"
            .to_owned(),
        ApiError::ResponseTooLarge { limit } => format!(
            "the Tracker API response is larger than {limit} bytes and was not returned. Narrow \
             the request: ask only for the fields you need (`fields`), use a smaller page \
             (`perPage`, `perScroll`) or stricter filters"
        ),
        ApiError::Transport(detail) => format!(
            "could not reach the Tracker API (network error or timeout); retry later. \
             Details: {detail}"
        ),
        ApiError::Io(detail) => format!("local file error: {detail}"),
        ApiError::LocalFileChanged(path) => format!(
            "local file `{path}` was replaced after its path was checked (for example, it or a \
             parent directory became a symbolic link), so it was not used and nothing was \
             written to it. Use a regular file inside the allowed directories"
        ),
    }
}

/// Текст ответа Tracker со статусом не 2xx: статус, подсказка и сообщения Tracker.
fn describe_http_error(err: &HttpError) -> String {
    let status = err.status;
    let hint = match status {
        300..=399 => {
            " (a redirect outside the Tracker API was not followed; the resource is probably \
             served from another host)"
        }
        400 | 422 => " (the request was rejected: check the arguments against the tool schema)",
        // Как заменить токен, зависит от режима сервера (хранилище или заголовок
        // HTTP-запроса), поэтому подробности — в инструкциях сервера.
        401 => {
            " (the token was rejected: it may be expired or revoked; ask the user to replace it \
             as described in the server instructions)"
        }
        403 => {
            " (the token's user has no permission for this operation or object, or the \
             organization ID does not match the token)"
        }
        404 => " (not found: check the keys and IDs, and that the token's user can see the object)",
        409 => {
            " (conflict with the current state of the object; for updates, re-read the object \
             and retry with its current version)"
        }
        412 => " (the object was changed by someone else; re-read it and retry)",
        429 => " (rate limited: retry later)",
        500..=599 => " (Tracker server error: retry later)",
        _ => "",
    };
    let mut details: Vec<String> = err.error_messages.clone();
    details.extend(err.errors.iter().map(|(field, message)| {
        if field.is_empty() {
            message.clone()
        } else {
            format!("{field}: {message}")
        }
    }));
    let details: Vec<&str> = details
        .iter()
        .map(|d| d.trim())
        .filter(|d| !d.is_empty())
        .collect();
    let mut text = format!("Tracker API returned HTTP {status}{hint}.");
    if !details.is_empty() {
        text.push_str(" Errors: ");
        text.push_str(&details.join("; "));
    } else if !err.body.trim().is_empty() {
        text.push_str(" Response body: ");
        text.push_str(err.body.trim());
    }
    if let Some(secs) = err.retry_after {
        text.push_str(&format!(" Retry after {secs} s."));
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn http(status: u16, messages: &[&str], errors: &[(&str, &str)], body: &str) -> ApiError {
        ApiError::Http(Box::new(HttpError {
            status,
            error_messages: messages.iter().map(|m| (*m).to_owned()).collect(),
            errors: errors
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            body: body.to_owned(),
            retry_after: None,
        }))
    }

    #[test]
    fn http_errors_have_status_hint_and_details() {
        let text = describe_api_error(&http(
            404,
            &["Задача не существует."],
            &[],
            r#"{"errorMessages":["Задача не существует."]}"#,
        ));
        assert!(text.contains("HTTP 404"), "{text}");
        assert!(text.contains("not found: check the keys"), "{text}");
        assert!(text.contains("Errors: Задача не существует."), "{text}");
        assert!(!text.contains("Response body"), "{text}");

        let text = describe_api_error(&http(400, &[], &[("queue", "required")], "{}"));
        assert!(text.contains("Errors: queue: required"), "{text}");

        let text = describe_api_error(&http(401, &[], &[], " "));
        assert!(text.contains("server instructions"), "{text}");
        assert!(!text.contains("Response body"), "{text}");

        let text = describe_api_error(&http(502, &[], &[], "Bad Gateway"));
        assert!(text.contains("retry later"), "{text}");
        assert!(text.contains("Response body: Bad Gateway"), "{text}");

        let mut limited = HttpError {
            status: 429,
            error_messages: Vec::new(),
            errors: BTreeMap::new(),
            body: String::new(),
            retry_after: Some(40),
        };
        let text = describe_api_error(&ApiError::Http(Box::new(limited.clone())));
        assert!(text.contains("Retry after 40 s"), "{text}");
        limited.retry_after = None;
        assert!(!describe_api_error(&ApiError::Http(Box::new(limited))).contains("Retry after"));
    }

    #[test]
    fn other_errors_are_english() {
        assert_eq!(
            describe_api_error(&ApiError::MissingPathParam("issueId".into())),
            "missing path argument `issueId`"
        );
        assert!(
            describe_api_error(&ApiError::InvalidPath("детали".into())).contains("valid API path")
        );
        let text = describe_api_error(&ApiError::InvalidConfig("пустой токен".into()));
        assert!(!text.contains("пустой"), "{text}");
        assert!(describe_api_error(&ApiError::MissingOrg).contains("--org-id"));
        let text = describe_api_error(&ApiError::ResponseTooLarge { limit: 1024 });
        assert!(
            text.contains("1024 bytes") && text.contains("`fields`"),
            "{text}"
        );
        assert!(
            describe_api_error(&ApiError::Transport("timeout".into()))
                .contains("could not reach the Tracker API")
        );
    }
}
