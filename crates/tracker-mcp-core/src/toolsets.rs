//! Наборы инструментов (`--toolsets`).
//!
//! Набор — раздел справочника (тег спецификации) с коротким английским slug из
//! `tracker_api::spec::toolsets()`. Набор выбирается точным совпадением slug; `all` включает
//! все наборы. Неизвестный набор — ошибка запуска со списком допустимых.
//!
//! Без выбора включается [`DEFAULT_TOOLSETS`]: основная работа с задачами (задачи, очереди,
//! доски и спринты, пользователи). Остальные разделы (администрирование, импорт, макросы,
//! массовые операции, дашборды, фильтры, проекты и цели, отсутствия) нужны реже, а их
//! описания заметно увеличивают список инструментов в контексте агента.

use tracker_api::spec::{find_toolset, operations, toolsets};

/// Значение `--toolsets`, которое включает все наборы.
pub const ALL_TOOLSETS: &str = "all";

/// Наборы, включённые без `--toolsets` и без настройки профиля.
pub const DEFAULT_TOOLSETS: &[&str] = &["issues", "queues", "boards", "users"];

/// Набор инструментов: slug, раздел справочника и число операций в спецификации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    pub slug: &'static str,
    pub tag: &'static str,
    pub description: &'static str,
    pub operations: usize,
    /// Входит в [`DEFAULT_TOOLSETS`].
    pub default: bool,
}

/// Все наборы в порядке разделов справочника.
pub fn available_toolsets() -> Vec<Toolset> {
    toolsets()
        .iter()
        .map(|toolset| Toolset {
            slug: toolset.slug,
            tag: toolset.tag,
            description: toolset.description,
            operations: operations()
                .iter()
                .filter(|op| op.toolset == toolset.slug)
                .count(),
            default: DEFAULT_TOOLSETS.contains(&toolset.slug),
        })
        .collect()
}

/// Проверяет имена наборов: пробелы по краям и пустые элементы отбрасываются, повторы
/// убираются, `all` раскрывается во все наборы. Неизвестный набор — ошибка.
pub fn normalize_toolsets<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>, String> {
    let mut result: Vec<String> = Vec::new();
    for name in names {
        let name = name.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if name == ALL_TOOLSETS {
            return Ok(toolsets().iter().map(|t| t.slug.to_owned()).collect());
        }
        if find_toolset(name).is_none() {
            let list: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
            return Err(format!(
                "unknown toolset `{name}`; available toolsets: {}, or `{ALL_TOOLSETS}`",
                list.join(", ")
            ));
        }
        if !result.iter().any(|known| known == name) {
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

/// Наборы, которые действуют при выборе `selected` (`None` или пусто — по умолчанию).
pub fn effective_toolsets(selected: Option<&[String]>) -> Vec<String> {
    match selected {
        Some(names) if !names.is_empty() => names.to_vec(),
        _ => DEFAULT_TOOLSETS.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_exist_and_cover_core_work() {
        for slug in DEFAULT_TOOLSETS {
            assert!(find_toolset(slug).is_some(), "{slug}");
        }
        let all = available_toolsets();
        assert_eq!(
            all.iter().map(|t| t.operations).sum::<usize>(),
            operations().len()
        );
        assert_eq!(
            all.iter().filter(|t| t.default).count(),
            DEFAULT_TOOLSETS.len()
        );
    }

    #[test]
    fn normalize_validates_names() {
        assert_eq!(
            normalize_toolsets(&[" issues ", "", "boards", "issues"]).unwrap(),
            ["issues", "boards"]
        );
        let err = normalize_toolsets(&["Задачи"]).unwrap_err();
        assert!(err.contains("unknown toolset `Задачи`"), "{err}");
        assert!(err.contains("issues") && err.contains("`all`"), "{err}");
        let all = normalize_toolsets(&["issues", "all"]).unwrap();
        assert_eq!(all.len(), toolsets().len());
        assert!(normalize_toolsets::<&str>(&[]).unwrap().is_empty());
    }

    #[test]
    fn effective_falls_back_to_defaults() {
        assert_eq!(effective_toolsets(None), DEFAULT_TOOLSETS);
        assert_eq!(effective_toolsets(Some(&[])), DEFAULT_TOOLSETS);
        let chosen = vec!["gaps".to_owned()];
        assert_eq!(effective_toolsets(Some(&chosen)), ["gaps"]);
    }
}
