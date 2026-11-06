//! Динамические наборы инструментов (`serve --dynamic-toolsets`).
//!
//! Клиенты без отложенной загрузки инструментов (например, Codex) кладут описания всех
//! инструментов в контекст сразу. В динамическом режиме при запуске регистрируются только
//! `api_search`, `api_request` и инструменты этого модуля: `list_toolsets`, `enable_toolset`,
//! `disable_toolset`, наборы из `--toolsets` (без него — ни одного) и пользовательские
//! инструменты: их явно настроил пользователь, и ни в один набор они не входят. Агент
//! включает нужный набор, сервер пересобирает реестр ([`ToolRegistry::with_toolsets`])
//! и сообщает клиенту `notifications/tools/list_changed`.
//!
//! Уровень доступа действует как обычно: включение набора не регистрирует инструменты выше
//! `max_access`, а в HTTP-режиме — инструменты с локальными файлами. Управление наборами
//! меняет только список инструментов этого сервера и данных Tracker не касается, поэтому
//! эти инструменты — уровня чтения.
//!
//! Здесь только описание инструментов, разбор аргументов и тексты ответов; состояние
//! (текущий реестр сессии) и уведомления — в [`crate::server`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use tracker_api::spec::{AccessLevel, operations};

use crate::error::ToolError;
use crate::registry::{ToolKind, ToolRegistry, access_flag, uses_local_files};
use crate::toolsets::{available_toolsets, normalize_toolsets};

pub const LIST_TOOLSETS_TOOL: &str = "list_toolsets";
pub const ENABLE_TOOLSET_TOOL: &str = "enable_toolset";
pub const DISABLE_TOOLSET_TOOL: &str = "disable_toolset";

/// Сколько имён инструментов показывать в `list_toolsets` как пример содержимого набора.
const EXAMPLES: usize = 4;

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

fn toolset_schema(verb: &str) -> JsonObject {
    object(json!({
        "type": "object",
        "properties": {
            "toolset": {
                "type": "string",
                "description": format!(
                    "Toolset to {verb} (slug from `{LIST_TOOLSETS_TOOL}`, e.g. `issues`). \
                     Several toolsets can be given separated by commas."
                )
            }
        },
        "required": ["toolset"],
        "additionalProperties": false
    }))
}

fn annotated(name: &'static str, title: &str, description: String, schema: JsonObject) -> Tool {
    let mut tool = Tool::new(name, description, Arc::new(schema));
    tool.title = Some(title.to_owned());
    tool.annotations = Some(
        ToolAnnotations::with_title(title)
            .read_only(true)
            .open_world(false),
    );
    tool
}

/// Описание `list_toolsets`.
pub fn list_toolsets_tool() -> Tool {
    annotated(
        LIST_TOOLSETS_TOOL,
        "List toolsets",
        format!(
            "List the toolsets (groups of Tracker API tools by reference section) of this \
             server: slug, section, number of tools available at the server's access level, \
             example tool names and whether the toolset is enabled. Only the tools of enabled \
             toolsets are loaded; enable a toolset with `{ENABLE_TOOLSET_TOOL}` before using \
             its tools. To find the toolset of a specific operation, use `api_search`.\n\n\
             Access: read-only (does not call the API)."
        ),
        object(json!({"type": "object", "properties": {}, "additionalProperties": false})),
    )
}

/// Описание `enable_toolset`.
pub fn enable_toolset_tool() -> Tool {
    annotated(
        ENABLE_TOOLSET_TOOL,
        "Enable a toolset",
        format!(
            "Load the tools of a toolset (see `{LIST_TOOLSETS_TOOL}`). The server then notifies \
             the client that its tool list changed, and the new tools become available. Tools \
             above the server's access level stay unavailable. If the new tools do not appear \
             in your tool list (the client does not reload it), call the same operations with \
             `api_request`, using the method and path from `api_search`.\n\n\
             Access: read-only (changes only the tool list of this server)."
        ),
        toolset_schema("enable"),
    )
}

/// Описание `disable_toolset`.
pub fn disable_toolset_tool() -> Tool {
    annotated(
        DISABLE_TOOLSET_TOOL,
        "Disable a toolset",
        format!(
            "Unload the tools of a toolset enabled earlier with `{ENABLE_TOOLSET_TOOL}`, to keep \
             the tool list short. `api_search`, `api_request` and custom tools stay \
             available.\n\n\
             Access: read-only (changes only the tool list of this server)."
        ),
        toolset_schema("disable"),
    )
}

/// Имена инструментов каждого набора, которые этот сервер может зарегистрировать (уровень
/// доступа и локальные файлы), без учёта того, включён ли набор.
fn tools_by_toolset(registry: &ToolRegistry) -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut map: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for op in operations() {
        if registry.max_access().allows(op.access)
            && (registry.local_files() || !uses_local_files(op))
        {
            map.entry(op.toolset).or_default().push(op.tool_name);
        }
    }
    map
}

/// Ответ `list_toolsets`: компактный JSON.
pub fn list(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    if let Some(key) = args.keys().next() {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; `{LIST_TOOLSETS_TOOL}` takes no arguments"
        )));
    }
    let available = tools_by_toolset(registry);
    let mut hidden_total = 0;
    let toolsets: Vec<Value> = available_toolsets()
        .into_iter()
        .map(|toolset| {
            let names = available.get(toolset.slug).cloned().unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("toolset".into(), json!(toolset.slug));
            entry.insert("section".into(), json!(toolset.tag));
            entry.insert("tools".into(), json!(names.len()));
            entry.insert(
                "examples".into(),
                json!(names.iter().take(EXAMPLES).collect::<Vec<_>>()),
            );
            // Операции выше уровня доступа: сколько их (включает их только пользователь).
            let hidden = operations()
                .iter()
                .filter(|op| op.toolset == toolset.slug && !registry.max_access().allows(op.access))
                .count();
            if hidden > 0 {
                hidden_total += hidden;
                entry.insert("above_access_level".into(), json!(hidden));
            }
            entry.insert(
                "enabled".into(),
                json!(registry.toolset_enabled(toolset.slug)),
            );
            Value::Object(entry)
        })
        .collect();
    let mut out = Map::new();
    out.insert("toolsets".into(), Value::Array(toolsets));
    let mut note = format!(
        "Enable a toolset with `{ENABLE_TOOLSET_TOOL}` to load its tools. `api_search`, \
         `api_request` and custom tools are always available."
    );
    if hidden_total > 0 {
        let flag = match registry.max_access() {
            AccessLevel::Read => access_flag(AccessLevel::Write),
            _ => access_flag(AccessLevel::Destructive),
        };
        note.push_str(&format!(
            " `above_access_level` counts operations this server does not allow at its access \
             level; only the user can allow them by restarting the server (e.g. with {flag})."
        ));
    }
    out.insert("note".into(), json!(note));
    Ok(Value::Object(out).to_string())
}

/// Наборы из аргумента `toolset` (через запятую), проверенные.
fn parse_toolsets(args: &JsonObject) -> Result<Vec<String>, ToolError> {
    if let Some(key) = args.keys().find(|key| key.as_str() != "toolset") {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; the only argument is `toolset`"
        )));
    }
    let raw = match args.get("toolset") {
        Some(Value::String(text)) => text,
        None | Some(Value::Null) => {
            return Err(ToolError::new(format!(
                "missing required argument `toolset`; see `{LIST_TOOLSETS_TOOL}` for the slugs"
            )));
        }
        Some(_) => return Err(ToolError::new("argument `toolset` must be a string")),
    };
    let names: Vec<&str> = raw.split(',').collect();
    let toolsets = normalize_toolsets(&names).map_err(ToolError::new)?;
    if toolsets.is_empty() {
        return Err(ToolError::new(format!(
            "argument `toolset` is empty; see `{LIST_TOOLSETS_TOOL}` for the slugs"
        )));
    }
    Ok(toolsets)
}

/// Имена зарегистрированных инструментов операций. Пользовательские инструменты ни в один
/// набор не входят.
fn toolset_tools(registry: &ToolRegistry) -> BTreeSet<String> {
    registry
        .iter()
        .filter(|tool| matches!(tool.kind, ToolKind::Operation(_)))
        .map(|tool| tool.tool.name.to_string())
        .collect()
}

/// Результат `enable_toolset`/`disable_toolset`: новый реестр (`None` — список инструментов
/// не изменился) и текст ответа.
#[derive(Debug)]
pub struct Toggle {
    pub registry: Option<ToolRegistry>,
    pub text: String,
}

/// Включает (`enable = true`) или выключает наборы из аргументов относительно реестра `current`.
pub fn toggle(
    current: &ToolRegistry,
    args: &JsonObject,
    enable: bool,
) -> Result<Toggle, ToolError> {
    let requested = parse_toolsets(args)?;
    let mut enabled: Vec<String> = current.enabled_toolsets().to_vec();
    let mut changed_sets = Vec::new();
    let mut unchanged_sets = Vec::new();
    for slug in requested {
        let present = enabled.contains(&slug);
        if present == enable {
            unchanged_sets.push(slug);
        } else {
            if enable {
                enabled.push(slug.clone());
            } else {
                enabled.retain(|name| name != &slug);
            }
            changed_sets.push(slug);
        }
    }

    let next = if changed_sets.is_empty() {
        None
    } else {
        Some(current.with_toolsets(&enabled).map_err(ToolError::new)?)
    };
    let before = toolset_tools(current);
    let after = next
        .as_ref()
        .map(toolset_tools)
        .unwrap_or_else(|| before.clone());
    let (diff_key, diff): (&str, Vec<&String>) = if enable {
        ("added_tools", after.difference(&before).collect())
    } else {
        ("removed_tools", before.difference(&after).collect())
    };

    let mut out = Map::new();
    let (changed_key, unchanged_key) = if enable {
        ("enabled", "already_enabled")
    } else {
        ("disabled", "not_enabled")
    };
    out.insert(changed_key.into(), json!(changed_sets));
    if !unchanged_sets.is_empty() {
        out.insert(unchanged_key.into(), json!(unchanged_sets));
    }
    out.insert(diff_key.into(), json!(diff));
    out.insert("enabled_toolsets".into(), json!(enabled));
    let note = if enable && !changed_sets.is_empty() && diff.is_empty() {
        Some(
            "No tools were added: all operations of this toolset are above the server's access \
             level (or need local files). Only the user can allow them by restarting the server."
                .to_owned(),
        )
    } else if enable && !diff.is_empty() {
        Some(
            "The client was notified that the tool list changed. If these tools do not appear \
             in your tool list, call the same operations with `api_request` (method and path \
             from `api_search`)."
                .to_owned(),
        )
    } else {
        None
    };
    if let Some(note) = note {
        out.insert("note".into(), json!(note));
    }
    Ok(Toggle {
        registry: next,
        text: Value::Object(out).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerOptions;

    fn registry(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            dynamic_toolsets: true,
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn args(value: Value) -> JsonObject {
        object(value)
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn initial_registry_has_only_core_tools() {
        // Без `--toolsets` в динамическом режиме — ни одного набора, а не набор по умолчанию.
        let reg = registry(AccessLevel::Read, None);
        let mut names: Vec<String> = reg.iter().map(|t| t.tool.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "api_request",
                "api_search",
                DISABLE_TOOLSET_TOOL,
                ENABLE_TOOLSET_TOOL,
                LIST_TOOLSETS_TOOL
            ]
        );
        assert!(reg.enabled_toolsets().is_empty());
        assert!(reg.iter().all(|t| t.access == AccessLevel::Read));
        for tool in reg.iter() {
            let annotations = tool.tool.annotations.as_ref().unwrap();
            assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.tool.name);
        }

        let started = registry(AccessLevel::Read, Some(&["boards"]));
        assert_eq!(started.enabled_toolsets(), ["boards"]);
        assert!(started.get("get_boards").is_some());
        assert!(started.get("get_issue").is_none());
    }

    #[test]
    fn list_shows_counts_and_state() {
        let reg = registry(AccessLevel::Read, Some(&["issues"]));
        let out = parse(&list(&JsonObject::new(), &reg).unwrap());
        let toolsets = out["toolsets"].as_array().unwrap();
        assert_eq!(toolsets.len(), available_toolsets().len());
        let issues = toolsets.iter().find(|t| t["toolset"] == "issues").unwrap();
        assert_eq!(issues["enabled"], true);
        assert!(issues["tools"].as_u64().unwrap() > 0);
        // Создание, изменение и удаление задач — выше уровня чтения.
        assert!(issues["above_access_level"].as_u64().unwrap() >= 1);
        let boards = toolsets.iter().find(|t| t["toolset"] == "boards").unwrap();
        assert_eq!(boards["enabled"], false);
        assert!(out["note"].as_str().unwrap().contains("--allow-write"));

        let err = list(&args(json!({"x": 1})), &reg).unwrap_err();
        assert!(err.message().contains("no arguments"));
    }

    #[test]
    fn enable_and_disable() {
        let reg = registry(AccessLevel::Read, None);
        let toggled = toggle(&reg, &args(json!({"toolset": "issues"})), true).unwrap();
        let next = toggled.registry.unwrap();
        assert!(next.get("get_issue").is_some());
        assert!(next.get("search_issues").is_some());
        assert!(next.get("create_issue").is_none(), "выше уровня доступа");
        let out = parse(&toggled.text);
        assert_eq!(out["enabled"], json!(["issues"]));
        assert!(
            out["added_tools"]
                .as_array()
                .unwrap()
                .contains(&json!("get_issue"))
        );

        // Повторное включение ничего не меняет.
        let again = toggle(&next, &args(json!({"toolset": "issues"})), true).unwrap();
        assert!(again.registry.is_none());
        assert_eq!(parse(&again.text)["already_enabled"], json!(["issues"]));

        let off = toggle(&next, &args(json!({"toolset": " issues ,boards"})), false).unwrap();
        let after = off.registry.unwrap();
        assert!(after.get("get_issue").is_none());
        let out = parse(&off.text);
        assert_eq!(out["disabled"], json!(["issues"]));
        assert_eq!(out["not_enabled"], json!(["boards"]));
        assert_eq!(out["enabled_toolsets"], json!([]));
    }

    #[test]
    fn rejects_bad_arguments() {
        let reg = registry(AccessLevel::Read, None);
        for (input, expected) in [
            (json!({}), "missing required argument"),
            (json!({"toolset": "repos"}), "unknown toolset `repos`"),
            (json!({"toolset": " , "}), "is empty"),
            (json!({"toolset": 1}), "must be a string"),
            (json!({"toolset": "issues", "x": 1}), "unknown argument `x`"),
        ] {
            let err = toggle(&reg, &args(input.clone()), true).unwrap_err();
            assert!(err.message().contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn disabled_reason_points_to_enable_toolset() {
        let reg = registry(AccessLevel::Read, None);
        let reason = reg.unavailable_reason("get_issue").unwrap();
        assert!(reason.contains("`enable_toolset`"), "{reason}");
        assert!(reason.contains("toolset: \"issues\""), "{reason}");
        assert!(reason.contains("api_request"), "{reason}");
        let reason = reg.unavailable_reason("create_issue").unwrap();
        assert!(reason.contains("--allow-write"), "{reason}");
    }

    #[test]
    fn remote_server_counts_only_tools_without_local_files() {
        let remote = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Destructive,
            dynamic_toolsets: true,
            local_machine: false,
            ..ServerOptions::default()
        })
        .unwrap();
        let next = toggle(&remote, &args(json!({"toolset": "issues"})), true)
            .unwrap()
            .registry
            .unwrap();
        assert!(next.get("get_issue").is_some());
        assert!(next.get("post_attachment").is_none());
        let out = parse(&list(&JsonObject::new(), &next).unwrap());
        let issues = out["toolsets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["toolset"] == "issues")
            .unwrap()
            .clone();
        let registered = next
            .iter()
            .filter(|t| matches!(t.kind, ToolKind::Operation(op) if op.toolset == "issues"))
            .count();
        assert_eq!(issues["tools"], json!(registered));
    }
}
