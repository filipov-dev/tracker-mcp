//! Сопоставление произвольного запроса (`api_request`) с операциями спецификации.
//!
//! Уровень доступа `api_request` определяется не только HTTP-методом: если запрос совпадает
//! с известной операцией, действует её уровень `x-access-level` (поиск через `POST …/_search`
//! и `…/_count` — чтение, изменение прав дашборда — удаление и т. д.).
//!
//! Правила сопоставления:
//!
//! - метод запроса должен совпадать с методом операции;
//! - сравнивается шаблон [`OperationDef::path`]; один завершающий `/` в пути запроса
//!   игнорируется;
//! - число сегментов совпадает; литеральный сегмент шаблона сравнивается без учёта регистра;
//! - первый сегмент — версия API: `/v2/…` совпадает и с шаблоном `/v3/…` (и наоборот), потому
//!   что большинство ресурсов Tracker доступно в обеих версиях, а в спецификации описана
//!   одна; совпадение с той же версией точнее;
//! - `{param}` совпадает с одним непустым сегментом; литеральные префикс и суффикс вокруг
//!   параметра обязательны, а значение между ними не может быть пустым;
//! - сегмент запроса сравнивается и в исходном, и в percent-раскодированном виде
//!   (до нескольких раундов), чтобы `_se%61rch` не обходил совпадение с `_search`.
//!
//! Если совпало несколько операций (например, `/v3/issues/_count` подходит и под
//! `/v3/issues/_count`, и под шаблон с параметром на том же месте), требуется максимальный
//! уровень доступа среди них: решение не зависит от того, как маршрутизирует запрос сервер
//! Tracker. Если не совпало ничего — уровень по методу ([`default_access`]).

use std::borrow::Cow;

use percent_encoding::percent_decode_str;
use tracker_api::spec::{AccessLevel, HttpMethod, OperationDef, operations};

/// Сколько раундов percent-раскодирования проверять (как в `validate_relative_path`).
const MAX_DECODE_ROUNDS: usize = 3;

/// Операция, с которой совпал запрос.
#[derive(Debug, Clone, Copy)]
pub struct RouteMatch {
    pub op: &'static OperationDef,
    /// Число литеральных символов шаблона, совпавших с запросом: чем больше, тем точнее.
    pub specificity: usize,
}

/// Уровень доступа запроса к пути, которого нет в спецификации: `GET` — чтение, `DELETE` —
/// удаление, остальные методы — запись.
pub fn default_access(method: HttpMethod) -> AccessLevel {
    match method {
        HttpMethod::Get => AccessLevel::Read,
        HttpMethod::Delete => AccessLevel::Destructive,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => AccessLevel::Write,
    }
}

/// Все операции с методом `method`, шаблон которых совпадает с `path`.
///
/// Порядок: сначала самые точные совпадения (больше литеральных символов), затем по имени.
pub fn match_operations(method: HttpMethod, path: &str) -> Vec<RouteMatch> {
    let segments = request_segments(path);
    let mut matches: Vec<RouteMatch> = operations()
        .iter()
        .filter(|op| op.method == method)
        .filter_map(|op| {
            template_specificity(op.path, &segments)
                .map(|specificity| RouteMatch { op, specificity })
        })
        .collect();
    matches.sort_by(|a, b| {
        b.specificity
            .cmp(&a.specificity)
            .then_with(|| a.op.tool_name.cmp(b.op.tool_name))
    });
    matches
}

/// Решение о доступе для запроса `api_request`.
#[derive(Debug, Clone)]
pub struct AccessDecision {
    /// Требуемый уровень доступа.
    pub required: AccessLevel,
    /// Совпавшие операции, самые точные первыми; пусто — уровень взят по методу.
    pub matches: Vec<RouteMatch>,
}

impl AccessDecision {
    /// Самая точная совпавшая операция.
    pub fn operation(&self) -> Option<&'static OperationDef> {
        self.matches.first().map(|m| m.op)
    }

    /// Совпала ли операция с именем инструмента `tool_name`.
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        self.matches.iter().any(|m| m.op.tool_name == tool_name)
    }

    /// Операция, на которой требуемый уровень достигается (самая точная из таких).
    pub fn strictest(&self) -> Option<&'static OperationDef> {
        self.matches
            .iter()
            .find(|m| m.op.access == self.required)
            .map(|m| m.op)
    }
}

/// Какой уровень доступа нужен запросу `method path`.
pub fn required_access(method: HttpMethod, path: &str) -> AccessDecision {
    let matches = match_operations(method, path);
    let required = matches
        .iter()
        .map(|m| m.op.access)
        .max()
        .unwrap_or_else(|| default_access(method));
    AccessDecision { required, matches }
}

/// Сегменты пути без ведущего и одного завершающего `/`.
fn request_segments(path: &str) -> Vec<&str> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('/').collect()
}

/// Точность совпадения шаблона с сегментами запроса; `None` — не совпадает.
fn template_specificity(template: &str, segments: &[&str]) -> Option<usize> {
    let template_segments = request_segments(template);
    if template_segments.len() != segments.len() || segments.is_empty() {
        return None;
    }
    let mut specificity = 0;
    for (index, (pattern, segment)) in template_segments.iter().zip(segments).enumerate() {
        if index == 0 {
            specificity += version_specificity(pattern, segment)?;
        } else {
            specificity += segment_specificity(pattern, segment)?;
        }
    }
    Some(specificity)
}

/// Версия API: та же версия — точное совпадение, другая из `v2`/`v3` — совпадение без очков.
fn version_specificity(pattern: &str, segment: &str) -> Option<usize> {
    let is_version = |value: &str| {
        tracker_api::path::API_VERSIONS
            .iter()
            .any(|v| v.eq_ignore_ascii_case(value))
    };
    if !is_version(pattern) {
        return segment_specificity(pattern, segment);
    }
    let variants = decoded_variants(segment);
    if variants
        .iter()
        .any(|value| value.eq_ignore_ascii_case(pattern))
    {
        Some(pattern.len())
    } else if variants.iter().any(|value| is_version(value)) {
        Some(0)
    } else {
        None
    }
}

/// Совпадение одного сегмента шаблона: литерал или `префикс{param}суффикс`.
fn segment_specificity(pattern: &str, segment: &str) -> Option<usize> {
    if segment.is_empty() {
        return None;
    }
    let variants = decoded_variants(segment);
    let Some((prefix, rest)) = pattern.split_once('{') else {
        return variants
            .iter()
            .any(|value| value.eq_ignore_ascii_case(pattern))
            .then_some(pattern.len());
    };
    // В шаблонах спецификации не больше одного параметра на сегмент (проверяется тестом).
    let suffix = rest.split_once('}').map_or("", |(_, suffix)| suffix);
    variants
        .iter()
        .any(|value| {
            value.len() > prefix.len() + suffix.len()
                && starts_with_ignore_case(value, prefix)
                && ends_with_ignore_case(value, suffix)
        })
        .then_some(prefix.len() + suffix.len())
}

/// Исходный сегмент и его percent-раскодированные варианты.
fn decoded_variants(segment: &str) -> Vec<Cow<'_, str>> {
    let mut variants = vec![Cow::Borrowed(segment)];
    for _ in 0..MAX_DECODE_ROUNDS {
        let Some(last) = variants.last() else { break };
        let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
        if decoded == **last {
            break;
        }
        variants.push(Cow::Owned(decoded));
    }
    variants
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| value.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(method: HttpMethod, path: &str) -> Vec<&'static str> {
        match_operations(method, path)
            .iter()
            .map(|m| m.op.tool_name)
            .collect()
    }

    fn first(method: HttpMethod, path: &str) -> Option<&'static str> {
        required_access(method, path)
            .operation()
            .map(|op| op.tool_name)
    }

    fn access(method: HttpMethod, path: &str) -> AccessLevel {
        required_access(method, path).required
    }

    /// Шаблон операции с подставленными значениями параметров.
    fn concrete(template: &str) -> String {
        let mut out = String::new();
        let mut rest = template;
        while let Some((head, tail)) = rest.split_once('{') {
            out.push_str(head);
            out.push_str("X1");
            rest = tail.split_once('}').map_or("", |(_, after)| after);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn templates_have_one_param_per_segment_and_a_version() {
        for op in operations() {
            let segments = request_segments(op.path);
            assert!(
                tracker_api::path::API_VERSIONS.contains(&segments[0]),
                "{}: {}",
                op.tool_name,
                op.path
            );
            for segment in segments {
                assert!(
                    segment.matches('{').count() <= 1,
                    "{}: {}",
                    op.tool_name,
                    op.path
                );
            }
        }
    }

    #[test]
    fn every_operation_matches_its_own_template() {
        for op in operations() {
            let path = concrete(op.path);
            let decision = required_access(op.method, &path);
            assert!(
                decision.matches_tool(op.tool_name),
                "{} не совпала с {path}",
                op.tool_name
            );
            assert!(decision.required >= op.access, "{}", op.tool_name);
        }
    }

    #[test]
    fn read_only_posts_stay_read_only() {
        // Поиск и подсчёт через POST не должны совпадать с изменяющими операциями: иначе
        // в режиме чтения `api_request` отказал бы в них.
        let read_posts: Vec<_> = operations()
            .iter()
            .filter(|op| op.method == HttpMethod::Post && op.access == AccessLevel::Read)
            .collect();
        assert!(read_posts.len() >= 5, "{}", read_posts.len());
        for op in read_posts {
            let path = concrete(op.path);
            assert_eq!(
                access(HttpMethod::Post, &path),
                AccessLevel::Read,
                "{}: {path} {:?}",
                op.tool_name,
                names(HttpMethod::Post, &path)
            );
        }
    }

    #[test]
    fn literal_segments_win_over_parameters() {
        assert_eq!(
            first(HttpMethod::Post, "/v3/issues/_search"),
            Some("search_issues")
        );
        assert_eq!(
            first(HttpMethod::Post, "/v3/issues/_count"),
            Some("count_issues")
        );
        assert_eq!(
            first(HttpMethod::Get, "/v3/issues/TEST-1"),
            Some("get_issue")
        );
        assert_eq!(
            first(HttpMethod::Get, "/v3/issues/TEST-1/worklog"),
            Some("issue_worklog")
        );
    }

    #[test]
    fn versions_are_interchangeable_but_exact_is_preferred() {
        // Поиск задач описан в v3, но доступен и через v2.
        assert_eq!(
            first(HttpMethod::Post, "/v2/issues/_search"),
            Some("search_issues")
        );
        assert_eq!(
            access(HttpMethod::Post, "/v2/issues/_search"),
            AccessLevel::Read
        );
        // Удаление фильтра описано только в v2.
        assert_eq!(
            first(HttpMethod::Delete, "/v2/filters/7"),
            Some("delete_filter")
        );
        assert_eq!(
            first(HttpMethod::Delete, "/v3/filters/7"),
            Some("delete_filter")
        );
        // Другой первый сегмент — не версия: совпадения нет.
        assert!(names(HttpMethod::Post, "/v4/issues/_search").is_empty());
    }

    #[test]
    fn unknown_paths_and_methods() {
        assert!(names(HttpMethod::Get, "/v3/no/such/endpoint").is_empty());
        assert!(names(HttpMethod::Get, "/").is_empty());
        assert!(names(HttpMethod::Get, "").is_empty());
        // Путь операции, но другой метод: совпадения нет.
        assert!(names(HttpMethod::Put, "/v3/issues/_search").is_empty());
    }

    #[test]
    fn trailing_slash_case_and_encoding() {
        assert_eq!(
            first(HttpMethod::Post, "/v3/issues/_search/"),
            Some("search_issues")
        );
        assert_eq!(
            first(HttpMethod::Post, "/V3/Issues/_SEARCH"),
            Some("search_issues")
        );
        assert_eq!(
            first(HttpMethod::Delete, "/v3/queues/TEST%2564"),
            Some("delete_queue")
        );
        assert_eq!(
            first(HttpMethod::Post, "/v3/issues/_se%61rch"),
            Some("search_issues")
        );
        assert_eq!(
            first(HttpMethod::Post, "/v3/issues/_se%2561rch"),
            Some("search_issues")
        );
        // Только один завершающий `/`.
        assert_eq!(first(HttpMethod::Get, "/v3/myself//"), None);
        assert_eq!(first(HttpMethod::Get, "/v3/myself/"), Some("get_user_info"));
    }

    #[test]
    fn access_comes_from_operation_then_method() {
        assert_eq!(access(HttpMethod::Post, "/v3/issues"), AccessLevel::Write);
        assert_eq!(
            access(HttpMethod::Post, "/v3/issues/_search"),
            AccessLevel::Read
        );
        assert_eq!(
            access(HttpMethod::Delete, "/v3/queues/TEST"),
            AccessLevel::Destructive
        );
        assert_eq!(
            access(HttpMethod::Post, "/v3/issues/TEST-1/worklog"),
            AccessLevel::Write
        );
        // Неизвестные пути — по методу.
        for (method, expected) in [
            (HttpMethod::Get, AccessLevel::Read),
            (HttpMethod::Post, AccessLevel::Write),
            (HttpMethod::Put, AccessLevel::Write),
            (HttpMethod::Patch, AccessLevel::Write),
            (HttpMethod::Delete, AccessLevel::Destructive),
        ] {
            assert_eq!(access(method, "/v3/unknown/thing"), expected, "{method:?}");
            assert_eq!(default_access(method), expected);
        }
    }

    #[test]
    fn strictest_names_the_operation_that_sets_the_level() {
        let decision = required_access(HttpMethod::Delete, "/v3/queues/TEST");
        assert_eq!(
            decision.strictest().map(|op| op.tool_name),
            Some("delete_queue")
        );
        assert!(
            required_access(HttpMethod::Get, "/v3/unknown")
                .strictest()
                .is_none()
        );
    }
}
