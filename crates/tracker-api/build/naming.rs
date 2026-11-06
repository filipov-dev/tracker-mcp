//! Имена MCP-инструментов.

/// Имя инструмента: `operationId` в snake_case (`getIssue` → `get_issue`).
///
/// Переименований нет: `operationId` в спецификации совпадают с именами методов
/// справочника, и агенту проще сопоставить инструмент со страницей справочника.
pub fn tool_name(operation_id: &str) -> String {
    to_snake_case(operation_id)
}

/// camelCase → snake_case с учётом аббревиатур: `getIssue` → `get_issue`,
/// `getURLInfo` → `get_url_info`, `viewAutoactionLogs2` → `view_autoaction_logs2`.
pub fn to_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(chars.len() + 8);
    for (index, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            let prev = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(|c| c.is_ascii_lowercase());
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_is_lower)
            {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Имя MCP-инструмента должно соответствовать `^[a-zA-Z0-9_-]{1,64}$`.
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
