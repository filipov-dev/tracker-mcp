//! Построение таблицы операций из спецификации и генерация Rust-кода.
//!
//! Этапы: наборы инструментов из `tags` → разбор операций (параметры, тело, ответ) →
//! JSON Schema аргументов → сортировка по имени инструмента → проверки → вывод
//! `TOOLSETS` и `OPERATIONS`.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::naming::{is_valid_tool_name, tool_name};
use crate::schema::{Resolver, deref, description_of};
use crate::toolsets::{TOOLSET_SLUGS, is_valid_slug, slug_for};

const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Заголовки организации. Их подставляет исполнитель из профиля, в аргументы
/// инструмента они не попадают.
const ORG_HEADERS: &[&str] = &["X-Org-ID", "X-Cloud-Org-ID"];

/// Ключи элемента `paths`, кроме методов, которые генератор понимает.
const PATH_ITEM_KEYS: &[&str] = &["parameters", "summary", "description"];

const JSON_MEDIA: &str = "application/json";
const MULTIPART_MEDIA: &str = "multipart/form-data";
const BINARY_MEDIA: &str = "application/octet-stream";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Body,
    FormData,
}

impl Location {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Path => "ParamLocation::Path",
            Self::Query => "ParamLocation::Query",
            Self::Body => "ParamLocation::Body",
            Self::FormData => "ParamLocation::FormData",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Json,
    Multipart,
}

impl BodyKind {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Json => "BodyKind::Json",
            Self::Multipart => "BodyKind::Multipart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseKind {
    Json,
    Binary,
}

impl ResponseKind {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Json => "ResponseKind::Json",
            Self::Binary => "ResponseKind::Binary",
        }
    }
}

#[derive(Debug, Clone)]
struct Param {
    name: String,
    location: Location,
    required: bool,
    description: String,
    /// Имя варианта `ParamType` в `spec.rs`.
    value_type: &'static str,
    /// JSON Schema значения без описания.
    schema: Value,
}

#[derive(Debug)]
struct Toolset {
    slug: &'static str,
    tag: String,
    description: String,
}

#[derive(Debug)]
struct Operation {
    tool_name: String,
    operation_id: String,
    toolset: &'static str,
    summary: String,
    description: String,
    docs_url: String,
    method: &'static str,
    path: String,
    access: &'static str,
    params: Vec<Param>,
    body: Option<BodyKind>,
    response: ResponseKind,
    input_schema: String,
}

/// Генерирует исходный код `operations.rs` из разобранной спецификации.
pub fn generate(spec: &Value) -> String {
    let toolsets = parse_toolsets(spec);
    let mut resolver = Resolver::new(spec);
    let mut operations = parse_operations(spec, &toolsets, &mut resolver);
    operations.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    check_invariants(&operations, &toolsets);
    emit(&toolsets, &operations)
}

/// Наборы инструментов в порядке `tags` спецификации.
fn parse_toolsets(spec: &Value) -> Vec<Toolset> {
    let tags = spec
        .get("tags")
        .and_then(Value::as_array)
        .expect("в спецификации нет tags");
    let mut toolsets = Vec::new();
    for tag in tags {
        let name = str_field(tag, "name").expect("у тега нет name");
        let slug = slug_for(&name).unwrap_or_else(|| {
            panic!("у тега «{name}» нет slug набора инструментов: добавь его в build/toolsets.rs")
        });
        toolsets.push(Toolset {
            slug,
            tag: name,
            description: str_field(tag, "description").unwrap_or_default(),
        });
    }
    for (tag, slug) in TOOLSET_SLUGS {
        assert!(is_valid_slug(slug), "недопустимый slug набора {slug}");
        assert!(
            toolsets.iter().any(|t| t.tag == *tag),
            "slug {slug} задан для тега «{tag}», которого нет в спецификации"
        );
    }
    let slugs: BTreeSet<&str> = toolsets.iter().map(|t| t.slug).collect();
    assert_eq!(slugs.len(), toolsets.len(), "slug наборов повторяются");
    toolsets
}

fn parse_operations(
    spec: &Value,
    toolsets: &[Toolset],
    resolver: &mut Resolver<'_>,
) -> Vec<Operation> {
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .expect("в спецификации нет paths");
    let mut operations = Vec::new();
    for (path, item) in paths {
        let item = item
            .as_object()
            .expect("элемент paths не является объектом");
        for key in item.keys() {
            assert!(
                METHODS.contains(&key.as_str()) || PATH_ITEM_KEYS.contains(&key.as_str()),
                "неподдерживаемый ключ {key} в пути {path}"
            );
        }
        let shared: Vec<&Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
        for method in METHODS {
            if let Some(op) = item.get(*method) {
                operations.push(parse_operation(
                    spec, toolsets, resolver, path, method, op, &shared,
                ));
            }
        }
    }
    operations
}

fn parse_operation(
    spec: &Value,
    toolsets: &[Toolset],
    resolver: &mut Resolver<'_>,
    path: &str,
    method: &'static str,
    op: &Value,
    shared_params: &[&Value],
) -> Operation {
    let operation_id =
        str_field(op, "operationId").unwrap_or_else(|| panic!("у {method} {path} нет operationId"));
    let tags: Vec<&str> = op
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let [tag] = tags.as_slice() else {
        panic!("у {operation_id} должен быть ровно один тег, а не {tags:?}");
    };
    let toolset = toolsets
        .iter()
        .find(|t| t.tag == *tag)
        .unwrap_or_else(|| panic!("{operation_id}: тег «{tag}» не объявлен в tags"))
        .slug;
    let access = match str_field(op, "x-access-level").as_deref() {
        Some("read") => "AccessLevel::Read",
        Some("write") => "AccessLevel::Write",
        Some("destructive") => "AccessLevel::Destructive",
        other => panic!("{operation_id}: неизвестный x-access-level {other:?}"),
    };
    let docs_url = op
        .get("externalDocs")
        .and_then(|docs| str_field(docs, "url"))
        .unwrap_or_else(|| panic!("{operation_id}: нет externalDocs.url"));

    let own_params = op
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let mut params = Vec::new();
    for raw in shared_params.iter().copied().chain(own_params) {
        if let Some(param) = parse_param(spec, &operation_id, raw) {
            params.push(param);
        }
    }
    let body = op
        .get("requestBody")
        .map(|raw| parse_body(spec, resolver, &operation_id, raw, &mut params));
    let response = response_kind(spec, &operation_id, op);

    for param in &mut params {
        param.description = finish_description(&param.name, &param.description);
    }
    check_path_params(&operation_id, path, &params);
    let input_schema = input_schema(&operation_id, &params);

    Operation {
        tool_name: tool_name(&operation_id),
        operation_id,
        toolset,
        summary: str_field(op, "summary").unwrap_or_default(),
        description: str_field(op, "description").unwrap_or_default(),
        docs_url,
        method,
        path: path.to_owned(),
        access,
        params,
        body,
        response,
        input_schema,
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
}

/// Параметр пути или запроса; `None` для заголовков организации.
fn parse_param(spec: &Value, operation_id: &str, raw: &Value) -> Option<Param> {
    let raw = deref(spec, raw);
    let object = raw.as_object().expect("параметр не является объектом");
    let name = str_field(raw, "name").expect("у параметра нет имени");
    let location = match str_field(raw, "in").as_deref() {
        Some("path") => Location::Path,
        Some("query") => Location::Query,
        Some("header") => {
            // Авторизацию и организацию задаёт только исполнитель: агент не должен
            // их видеть и переопределять. Другие заголовки генератор не поддерживает.
            assert!(
                ORG_HEADERS.contains(&name.as_str()),
                "{operation_id}: неподдерживаемый заголовок {name}"
            );
            return None;
        }
        other => panic!("{operation_id}: неподдерживаемое расположение параметра {other:?}"),
    };
    let required =
        location == Location::Path || raw.get("required").and_then(Value::as_bool) == Some(true);
    let schema = raw
        .get("schema")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: у параметра {name} нет schema"));
    let (schema, value_type) = simple_schema(operation_id, &name, schema);
    assert_ne!(
        value_type, "File",
        "{operation_id}: файл в параметре {name} вне multipart"
    );
    Some(Param {
        name,
        location,
        required,
        description: description_of(object).unwrap_or_default(),
        value_type,
        schema,
    })
}

/// Разбирает `requestBody`: JSON-тело становится параметром `body`, поля multipart —
/// параметрами `FormData`.
fn parse_body(
    spec: &Value,
    resolver: &mut Resolver<'_>,
    operation_id: &str,
    raw: &Value,
    params: &mut Vec<Param>,
) -> BodyKind {
    let raw = deref(spec, raw);
    let body_required = raw.get("required").and_then(Value::as_bool) == Some(true);
    let content = raw
        .get("content")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: requestBody без content"));
    let [(media_type, media)] = content.iter().collect::<Vec<_>>()[..] else {
        panic!("{operation_id}: у requestBody должен быть ровно один тип содержимого");
    };
    let schema = media
        .get("schema")
        .unwrap_or_else(|| panic!("{operation_id}: requestBody без schema"));
    let schema = resolver.resolve(schema);
    match media_type.as_str() {
        JSON_MEDIA => {
            let own = schema
                .get("description")
                .and_then(Value::as_str)
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            params.push(Param {
                name: "body".to_owned(),
                location: Location::Body,
                required: body_required,
                description: format!("JSON request body.{own}"),
                value_type: "Object",
                schema,
            });
            BodyKind::Json
        }
        MULTIPART_MEDIA => {
            let required: BTreeSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("{operation_id}: multipart без properties"));
            for (name, property) in properties {
                let property = property
                    .as_object()
                    .unwrap_or_else(|| panic!("{operation_id}: поле multipart {name} не объект"));
                let (field_schema, value_type) = simple_schema(operation_id, name, property);
                let mut description = description_of(property).unwrap_or_default();
                if value_type == "File" {
                    description = file_description(name, &description);
                }
                params.push(Param {
                    name: name.clone(),
                    location: Location::FormData,
                    required: required.contains(name.as_str()),
                    description,
                    value_type,
                    schema: field_schema,
                });
            }
            BodyKind::Multipart
        }
        other => panic!("{operation_id}: неподдерживаемый тип requestBody {other}"),
    }
}

/// Вид успешного ответа: бинарный, если хоть один ответ `2xx` отдаёт `application/octet-stream`.
fn response_kind(spec: &Value, operation_id: &str, op: &Value) -> ResponseKind {
    let responses = op
        .get("responses")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: нет responses"));
    let mut kind = ResponseKind::Json;
    for (code, response) in responses {
        if !code.starts_with('2') {
            continue;
        }
        let response = deref(spec, response);
        for media_type in response
            .get("content")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(Map::keys)
        {
            match media_type.as_str() {
                JSON_MEDIA => {}
                BINARY_MEDIA => kind = ResponseKind::Binary,
                other => panic!("{operation_id}: неподдерживаемый тип ответа {other}"),
            }
        }
    }
    kind
}

/// Описание файлового поля: агент передаёт путь к файлу, а не его содержимое.
fn file_description(name: &str, description: &str) -> String {
    let note = format!(
        "Absolute path to a local file; the server reads it and uploads \
         its content as multipart field `{name}`."
    );
    let text = description.trim();
    if text.is_empty() {
        note
    } else if text.ends_with(['.', '!', '?', ':']) {
        format!("{text} {note}")
    } else {
        format!("{text}. {note}")
    }
}

/// Схема и тип простого параметра (path, query, поле multipart).
fn simple_schema(
    operation_id: &str,
    name: &str,
    object: &Map<String, Value>,
) -> (Value, &'static str) {
    // Без типа простой параметр передаётся строкой; тип-массив (`["string", "null"]`)
    // генератор не поддерживает.
    let kind = match object.get("type") {
        None => "string",
        Some(Value::String(kind)) => kind.as_str(),
        Some(other) => panic!("{operation_id}: неподдерживаемый тип {other} параметра {name}"),
    };
    if object.get("format").and_then(Value::as_str) == Some("binary") {
        return (json!({ "type": "string" }), "File");
    }
    let value_type = match kind {
        "string" => "String",
        "integer" => "Integer",
        "number" => "Number",
        "boolean" => "Boolean",
        "array" => "Array",
        other => panic!("{operation_id}: неподдерживаемый тип {other} параметра {name}"),
    };
    let mut schema = Map::new();
    schema.insert("type".into(), json!(kind));
    for key in ["enum", "default", "minimum", "maximum"] {
        if let Some(value) = object.get(key) {
            schema.insert(key.into(), value.clone());
        }
    }
    if let Some(items) = object.get("items").and_then(Value::as_object) {
        let (items, item_type) = simple_schema(operation_id, name, items);
        assert!(
            !matches!(item_type, "Array" | "File"),
            "{operation_id}: неподдерживаемый элемент массива {name}"
        );
        schema.insert("items".into(), items);
    } else if value_type == "Array" {
        // Элементы массива в пути и строке запроса всё равно уходят текстом.
        schema.insert("items".into(), json!({ "type": "string" }));
    }
    (Value::Object(schema), value_type)
}

/// Итоговое описание параметра: текст спецификации или имя параметра.
fn finish_description(name: &str, description: &str) -> String {
    let text = description.trim();
    if text.is_empty() {
        name.to_owned()
    } else {
        text.to_owned()
    }
}

/// Проверяет, что плейсхолдеры `{name}` пути совпадают с path-параметрами.
fn check_path_params(operation_id: &str, path: &str, params: &[Param]) {
    let placeholders: BTreeSet<&str> = path
        .split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect();
    let declared: BTreeSet<&str> = params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        placeholders, declared,
        "{operation_id}: плейсхолдеры пути {path} не совпадают с path-параметрами"
    );
}

/// JSON Schema аргументов инструмента.
fn input_schema(operation_id: &str, params: &[Param]) -> String {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in params {
        let mut schema = param.schema.clone();
        let object = schema
            .as_object_mut()
            .expect("схема параметра не является объектом");
        object.insert(
            "description".into(),
            Value::String(param.description.clone()),
        );
        assert!(
            properties.insert(param.name.clone(), schema).is_none(),
            "{operation_id}: параметр {} повторяется",
            param.name
        );
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    serde_json::to_string(&Value::Object(schema)).expect("сериализация JSON Schema")
}

fn check_invariants(operations: &[Operation], toolsets: &[Toolset]) {
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for op in operations {
        assert!(
            is_valid_tool_name(&op.tool_name),
            "недопустимое имя инструмента {}",
            op.tool_name
        );
        assert!(
            names.insert(op.tool_name.as_str()),
            "имя инструмента {} повторяется ({})",
            op.tool_name,
            op.operation_id
        );
        assert!(
            ids.insert(op.operation_id.as_str()),
            "operationId {} повторяется",
            op.operation_id
        );
    }
    for toolset in toolsets {
        assert!(
            operations.iter().any(|op| op.toolset == toolset.slug),
            "в наборе {} нет операций",
            toolset.slug
        );
    }
}

fn rust_method(method: &str) -> &'static str {
    match method {
        "get" => "HttpMethod::Get",
        "post" => "HttpMethod::Post",
        "put" => "HttpMethod::Put",
        "patch" => "HttpMethod::Patch",
        "delete" => "HttpMethod::Delete",
        other => panic!("неподдерживаемый метод {other}"),
    }
}

/// Строковые литералы выводятся через `{:?}`: это корректное экранирование для Rust.
fn emit(toolsets: &[Toolset], operations: &[Operation]) -> String {
    let mut out = String::new();
    out.push_str("// Сгенерировано build.rs из spec/tracker.openapi.json. Не редактировать.\n");
    out.push_str("pub static TOOLSETS: &[ToolsetDef] = &[\n");
    for toolset in toolsets {
        let _ = writeln!(
            out,
            "    ToolsetDef {{ slug: {:?}, tag: {:?}, description: {:?} }},",
            toolset.slug, toolset.tag, toolset.description
        );
    }
    out.push_str("];\n\n");
    out.push_str("pub static OPERATIONS: &[OperationDef] = &[\n");
    for op in operations {
        let _ = writeln!(out, "    OperationDef {{");
        let _ = writeln!(out, "        tool_name: {:?},", op.tool_name);
        let _ = writeln!(out, "        operation_id: {:?},", op.operation_id);
        let _ = writeln!(out, "        toolset: {:?},", op.toolset);
        let _ = writeln!(out, "        summary: {:?},", op.summary);
        let _ = writeln!(out, "        description: {:?},", op.description);
        let _ = writeln!(out, "        docs_url: {:?},", op.docs_url);
        let _ = writeln!(out, "        method: {},", rust_method(op.method));
        let _ = writeln!(out, "        path: {:?},", op.path);
        let _ = writeln!(out, "        access: {},", op.access);
        let _ = writeln!(out, "        params: &[");
        for param in &op.params {
            let _ = writeln!(
                out,
                "            ParamDef {{ name: {:?}, location: {}, required: {}, description: {:?}, value_type: ParamType::{} }},",
                param.name,
                param.location.rust_name(),
                param.required,
                param.description,
                param.value_type,
            );
        }
        let _ = writeln!(out, "        ],");
        let body = op.body.map_or_else(
            || "None".to_owned(),
            |kind| format!("Some({})", kind.rust_name()),
        );
        let _ = writeln!(out, "        body: {body},");
        let _ = writeln!(out, "        response: {},", op.response.rust_name());
        let _ = writeln!(out, "        input_schema: {:?},", op.input_schema);
        let _ = writeln!(out, "    }},");
    }
    out.push_str("];\n");
    out
}
