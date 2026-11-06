//! Проверка и сборка относительных путей Tracker API.
//!
//! Это граница безопасности: всё, что уходит в [`crate::executor::ApiClient::execute`],
//! проходит через [`validate_relative_path`], поэтому запрос не может уйти на другой хост
//! или выйти за пределы API через `..`, в том числе в percent-кодированном виде.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::executor::ApiError;

/// Версии API, с которых обязан начинаться путь: `/v2/...` или `/v3/...`.
pub const API_VERSIONS: [&str; 2] = ["v2", "v3"];

/// Символы, которые кодируются в значении параметра пути.
///
/// Оставляем только unreserved-символы RFC 3986, поэтому `/`, `?`, `#`, `%`, `:` и прочие
/// разделители внутри значения становятся `%XX` и не могут изменить структуру пути.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Сколько раз раскодировать сегмент при проверке. Защищает от двойного кодирования
/// (`%252e%252e`) на случай, если что-то между нами и API раскодирует путь повторно.
const MAX_DECODE_ROUNDS: usize = 3;

/// Проверяет относительный путь Tracker API, например `/v3/issues/TEST-1/comments`.
///
/// Требования:
/// - начинается с `/v2/` или `/v3/` (см. [`API_VERSIONS`]), поэтому не может быть `//`,
///   схемой или хостом;
/// - содержит только символы `pchar` из RFC 3986 и `/`: `?`, `#`, `\`, пробелы,
///   управляющие и не-ASCII символы отклоняются (query передаётся отдельно);
/// - `%` всегда образует корректную тройку `%XX`;
/// - нет пустых сегментов (`//`) кроме завершающего `/`;
/// - ни один сегмент ни в исходном, ни в раскодированном виде не равен `.` или `..`,
///   а раскодированный сегмент не содержит `.`/`..` между `/` или `\` и управляющих символов.
pub fn validate_relative_path(path: &str) -> Result<(), ApiError> {
    let invalid = |reason: &str| {
        Err(ApiError::InvalidPath(format!(
            "{reason}: `{}`",
            shorten(path)
        )))
    };

    if !path.starts_with('/') {
        return invalid("путь должен начинаться с `/`");
    }
    if path.starts_with("//") {
        return invalid("путь не может начинаться с `//`");
    }
    if let Some(c) = path.chars().find(|c| !is_allowed_char(*c)) {
        return Err(ApiError::InvalidPath(format!(
            "недопустимый символ {c:?} в пути `{}`; query передаётся отдельно, \
             остальные символы кодируйте как %XX",
            shorten(path)
        )));
    }
    if !has_valid_percent_triplets(path) {
        return invalid("некорректная percent-последовательность");
    }

    let segments: Vec<&str> = path[1..].split('/').collect();
    let version_ok =
        segments.len() >= 2 && API_VERSIONS.contains(&segments[0]) && !segments[1].is_empty();
    if !version_ok {
        return invalid("путь должен начинаться с `/v2/` или `/v3/` и указывать ресурс");
    }
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // Пустой сегмент допустим только в конце (`/v3/issues/`).
            if index == last {
                continue;
            }
            return invalid("пустой сегмент пути");
        }
        if !segment_is_safe(segment) {
            return invalid("сегменты `.` и `..` запрещены, в том числе в закодированном виде");
        }
    }
    Ok(())
}

/// Подставляет параметры в шаблон пути вида `/v3/issues/{issue_id}/comments/{comment_id}`
/// и проверяет результат через [`validate_relative_path`].
///
/// Значение параметра кодируется как один сегмент пути: `a/b` превращается в `a%2Fb`,
/// поэтому значение не может добавить сегменты или выйти из пути. Отсутствующий или
/// пустой параметр даёт [`ApiError::MissingPathParam`]. Значения `.` и `..` отклоняются
/// проверкой итогового пути.
pub fn render_path<F>(template: &str, mut lookup: F) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(ApiError::InvalidPath(format!(
                "лишняя `}}` в шаблоне `{}`",
                shorten(template)
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(ApiError::InvalidPath(format!(
                "незакрытая `{{` в шаблоне `{}`",
                shorten(template)
            )));
        };
        let name = &after[..close];
        if name.is_empty() || name.contains('{') {
            return Err(ApiError::InvalidPath(format!(
                "некорректный параметр в шаблоне `{}`",
                shorten(template)
            )));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::MissingPathParam(name.to_string()))?;
        out.extend(utf8_percent_encode(&value, SEGMENT));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    validate_relative_path(&out)?;
    Ok(out)
}

/// `pchar` из RFC 3986 плюс разделитель `/`; `%` проверяется отдельно.
fn is_allowed_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '/'
                | '%'
        )
}

fn has_valid_percent_triplets(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let ok = bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !ok {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Проверяет сегмент и его раскодированные варианты.
///
/// Закодированный `/` внутри значения (`a%2Fb`) допустим — так `render_path` передаёт
/// значения со слешем. Но если сервер раскодирует `%2F` до маршрутизации, `..%2F..`
/// превратится в обход каталога, поэтому раскодированный сегмент дополнительно делим
/// по `/` и `\` и ищем `.`/`..`.
fn segment_is_safe(segment: &str) -> bool {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if current
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            return false;
        }
        if current.chars().any(char::is_control) {
            return false;
        }
        if !current.contains('%') {
            return true;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    // Слишком глубокое вложенное кодирование — подозрительно, отклоняем.
    false
}

/// Укорачивает путь для текста ошибки, чтобы не раздувать ответ MCP.
fn shorten(path: &str) -> String {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path.escape_debug().to_string();
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", path[..end].escape_debug())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn accepts_valid_paths() {
        for path in [
            "/v3/myself",
            "/v2/myself",
            "/v3/issues/TEST-1/comments",
            "/v3/issues/_search",
            "/v3/queues/TEST/",
            "/v3/issues/TEST-1/attachments/123/file%20name.png",
            "/v3/a/b..c/.hidden/d.",
            "/v3/entities/project/id:1@x",
        ] {
            assert!(
                validate_relative_path(path).is_ok(),
                "{path} должен проходить"
            );
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/",
            "/v3",
            "/v3/",
            "/v1/issues",
            "/v4/issues",
            "/V3/issues",
            "/issues",
            "v3/issues",
            "https://evil.example/v3/issues",
            "http://evil.example",
            "//evil.example/v3/issues",
            "///evil",
            "/v3//issues",
            "/v3/issues/../admin",
            "/v3/../v2/issues",
            "/v3/./x",
            "/v3/issues/..",
            "/v3/issues/%2e%2e/admin",
            "/v3/issues/%2E%2E/admin",
            "/v3/issues/.%2e/admin",
            "/v3/issues/%2e/admin",
            "/v3/issues/..%2fadmin",
            "/v3/issues/x%2F..%2Fadmin",
            "/v3/issues/..%5cadmin",
            "/v3/issues/%252e%252e/admin",
            "/v3/issues/%25252e%25252e/admin",
            "/v3/issues?x=1",
            "/v3/issues#frag",
            "/v3/issues\\..\\admin",
            "/v3/issues/a b",
            "/v3/issues/\n",
            "/v3/issues/%00",
            "/v3/issues/%0a",
            "/v3/issues/%zz",
            "/v3/issues/%2",
            "/v3/issues/{key}",
            "/v3/issues/кириллица",
            "/v3/issues/x\u{0}",
        ] {
            assert!(
                matches!(validate_relative_path(path), Err(ApiError::InvalidPath(_))),
                "{path:?} должен отклоняться"
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_and_encodes() {
        let p = params(&[
            ("issue_id", "TEST-1"),
            ("attachment_id", "a/b"),
            ("filename", "K?#% x.png"),
        ]);
        let rendered = render_path(
            "/v3/issues/{issue_id}/attachments/{attachment_id}/{filename}",
            |n| p.get(n).cloned(),
        )
        .unwrap();
        assert_eq!(
            rendered,
            "/v3/issues/TEST-1/attachments/a%2Fb/K%3F%23%25%20x.png"
        );
    }

    #[test]
    fn render_encodes_non_ascii() {
        let p = params(&[("name", "файл")]);
        let rendered = render_path("/v3/files/{name}", |n| p.get(n).cloned()).unwrap();
        assert_eq!(rendered, "/v3/files/%D1%84%D0%B0%D0%B9%D0%BB");
    }

    #[test]
    fn render_reports_missing_and_empty_params() {
        let p = params(&[("issue_id", "TEST-1"), ("comment_id", "")]);
        let lookup = |n: &str| p.get(n).cloned();
        assert!(matches!(
            render_path("/v3/issues/{issue_id}/{missing}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "missing"
        ));
        assert!(matches!(
            render_path("/v3/issues/{issue_id}/comments/{comment_id}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "comment_id"
        ));
    }

    #[test]
    fn render_rejects_traversal_values() {
        for value in ["..", ".", "../admin", "..\\x"] {
            let result = render_path("/v3/issues/{x}/comments", |_| Some(value.to_string()));
            assert!(
                matches!(result, Err(ApiError::InvalidPath(_))),
                "значение {value:?} должно отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_malformed_templates() {
        for template in [
            "/v3/issues/{key",
            "/v3/issues/key}",
            "/v3/issues/{}",
            "/v3/issues/{{x}}",
        ] {
            assert!(
                matches!(
                    render_path(template, |_| Some("v".into())),
                    Err(ApiError::InvalidPath(_))
                ),
                "{template} должен отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_absolute_template() {
        assert!(render_path("https://evil/{x}", |_| Some("v".into())).is_err());
        // Значение параметра в версии не может подменить версию API.
        assert!(render_path("/{version}/issues", |_| Some("v1".into())).is_err());
    }
}
