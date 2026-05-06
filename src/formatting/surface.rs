// Surface-aware formatters.
//
// Render `ScannedModule` and JSON Schema for specific consumer surfaces:
// LLM context (markdown), agent skill files (skill), CLI listings (table-row),
// and programmatic APIs (json). See
// `apcore-toolkit/docs/features/formatting.md`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use apcore::module::ModuleAnnotations;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::serializers::{annotations_to_dict, module_to_dict};
use crate::types::ScannedModule;

/// Snake-case Map of every default-valued annotation field.
///
/// Used by the behavior-table renderer to skip fields that match the
/// protocol default, keeping the table focused on what is actually
/// non-default about the module. Lazily initialised on first access; the
/// `ModuleAnnotations::default()` value never changes within a process so
/// caching it is safe.
fn default_annotations_dict() -> &'static Map<String, Value> {
    static CACHE: OnceLock<Map<String, Value>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let default_ann = ModuleAnnotations::default();
        match annotations_to_dict(Some(&default_ann)) {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    })
}

/// Schema render style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaStyle {
    /// Markdown bullet list, one line per top-level property.
    Prose,
    /// Markdown pipe table.
    Table,
    /// Pass-through JSON.
    Json,
}

/// Module render style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleStyle {
    /// LLM context — sections + parameter prose + behavior facts.
    Markdown,
    /// `markdown` body prefixed with minimal `name` + `description` YAML
    /// frontmatter (vendor-neutral SKILL.md form).
    Skill,
    /// CLI listing — single pipe-separated row.
    TableRow,
    /// Pass-through `module_to_dict`.
    Json,
}

/// Group-by axis for [`format_modules`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// Group by `ScannedModule.tags` (modules in multiple tags appear
    /// multiple times; untagged modules go into `(untagged)`).
    Tag,
    /// Group by everything before the first `.` in `module_id`.
    Prefix,
}

/// Polymorphic return for the surface formatters.
///
/// `Markdown` / `Skill` / `TableRow` styles return `Text`; `Json`
/// returns `Value` (single module) or `Values` (module list).
#[derive(Debug, Clone)]
pub enum FormatOutput {
    Text(String),
    Value(Value),
    Values(Vec<Value>),
}

impl FormatOutput {
    /// Borrow as a string slice if this is a [`FormatOutput::Text`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FormatOutput::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Borrow as a single JSON value if this is a [`FormatOutput::Value`].
    pub fn as_value(&self) -> Option<&Value> {
        match self {
            FormatOutput::Value(v) => Some(v),
            _ => None,
        }
    }

    /// Borrow as a JSON array if this is a [`FormatOutput::Values`].
    pub fn as_values(&self) -> Option<&[Value]> {
        match self {
            FormatOutput::Values(v) => Some(v),
            _ => None,
        }
    }
}

/// Error returned by the surface formatters.
#[derive(Debug, Error)]
pub enum FormatError {
    #[error("formatSchema: schema must be a JSON object, got {0}")]
    SchemaNotObject(&'static str),
}

const DEFAULT_MAX_DEPTH: usize = 3;

/// Render a JSON Schema for a specific surface.
///
/// See `Contract: format_schema` in
/// `apcore-toolkit/docs/features/formatting.md`.
pub fn format_schema(schema: &Value, style: SchemaStyle, max_depth: Option<usize>) -> FormatOutput {
    let max_depth = max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    match style {
        SchemaStyle::Json => FormatOutput::Value(schema.clone()),
        SchemaStyle::Prose => FormatOutput::Text(render_schema_prose(schema, max_depth, 0)),
        SchemaStyle::Table => FormatOutput::Text(render_schema_table(schema)),
    }
}

fn render_schema_prose(schema: &Value, max_depth: usize, depth: usize) -> String {
    let Some(obj) = schema.as_object() else {
        return String::new();
    };
    let type_ = obj.get("type").and_then(|v| v.as_str());
    let properties = obj.get("properties").and_then(|v| v.as_object());
    if type_ != Some("object") || properties.is_none() {
        if let Some(t) = type_ {
            if t != "object" {
                return format!("_schema accepts {t}_");
            }
        }
        return String::new();
    }
    let properties = properties.unwrap();
    let required = required_set(obj);
    render_properties_prose(properties, &required, max_depth, depth)
}

fn render_properties_prose(
    properties: &Map<String, Value>,
    required: &std::collections::HashSet<String>,
    max_depth: usize,
    depth: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (name, prop) in properties.iter() {
        let prop_obj = prop.as_object();
        let type_ = prop_obj
            .and_then(|o| o.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("any");
        let req_label = if required.contains(name) {
            "required"
        } else {
            "optional"
        };
        let desc = prop_obj
            .and_then(|o| o.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let mut head = format!("- `{name}` ({type_}, {req_label})");
        if !desc.is_empty() {
            head.push_str(" — ");
            head.push_str(desc);
        }
        lines.push(head);

        if let Some(prop_obj) = prop_obj {
            if prop_obj.get("type").and_then(|v| v.as_str()) == Some("object") {
                if let Some(nested_props) = prop_obj.get("properties").and_then(|v| v.as_object()) {
                    if depth + 1 >= max_depth {
                        lines.push("  ```json".to_string());
                        let pretty =
                            serde_json::to_string_pretty(prop).unwrap_or_else(|_| "{}".to_string());
                        for line in pretty.lines() {
                            lines.push(format!("  {line}"));
                        }
                        lines.push("  ```".to_string());
                    } else {
                        let nested_required = required_set(prop_obj);
                        let nested = render_properties_prose(
                            nested_props,
                            &nested_required,
                            max_depth,
                            depth + 1,
                        );
                        for line in nested.lines() {
                            lines.push(format!("  {line}"));
                        }
                    }
                }
            }
        }
    }
    lines.join("\n")
}

fn render_schema_table(schema: &Value) -> String {
    let Some(obj) = schema.as_object() else {
        return String::new();
    };
    let type_ = obj.get("type").and_then(|v| v.as_str());
    let properties = obj.get("properties").and_then(|v| v.as_object());
    if type_ != Some("object") || properties.is_none() {
        if let Some(t) = type_ {
            if t != "object" {
                return format!("_schema accepts {t}_");
            }
        }
        return "| Name | Type | Required | Default | Description |\n|---|---|---|---|---|\n"
            .to_string();
    }
    let properties = properties.unwrap();
    let required = required_set(obj);
    let mut rows: Vec<String> = vec![
        "| Name | Type | Required | Default | Description |".to_string(),
        "|---|---|---|---|---|".to_string(),
    ];
    for (name, prop) in properties.iter() {
        let prop_obj = prop.as_object();
        let type_ = prop_obj
            .and_then(|o| o.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("any");
        let req_label = if required.contains(name) { "yes" } else { "no" };
        let desc = prop_obj
            .and_then(|o| o.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let default_str = prop_obj
            .and_then(|o| o.get("default"))
            .map(|v| {
                if v.is_string() {
                    v.as_str().unwrap_or("").to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default();
        rows.push(format!(
            "| `{name}` | {type_} | {req_label} | {default_str} | {desc} |"
        ));
    }
    rows.join("\n")
}

fn required_set(obj: &Map<String, Value>) -> std::collections::HashSet<String> {
    obj.get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Render a single ScannedModule for the chosen surface.
///
/// See `Contract: format_module` in
/// `apcore-toolkit/docs/features/formatting.md`.
pub fn format_module(module: &ScannedModule, style: ModuleStyle, display: bool) -> FormatOutput {
    if matches!(style, ModuleStyle::Json) {
        return FormatOutput::Value(module_to_dict(module));
    }

    let resolved = resolve_display_fields(module, display);

    if matches!(style, ModuleStyle::TableRow) {
        let alias = if resolved.title != module.module_id {
            resolved.title.clone()
        } else {
            String::new()
        };
        let tag_str = if resolved.tags.is_empty() {
            String::new()
        } else {
            resolved.tags.join(", ")
        };
        let line = format!(
            "`{}` │ `{}` │ {} │ {}",
            module.module_id, alias, resolved.description, tag_str
        );
        return FormatOutput::Text(line);
    }

    let body = render_module_markdown_body(module, &resolved);

    match style {
        ModuleStyle::Skill => {
            let one_line = resolved.description.replace('\n', " ");
            let one_line = one_line.trim();
            let frontmatter = format!(
                "---\nname: {}\ndescription: {}\n---\n\n",
                resolved.title,
                yaml_scalar(one_line)
            );
            FormatOutput::Text(frontmatter + &body)
        }
        ModuleStyle::Markdown => FormatOutput::Text(body),
        ModuleStyle::TableRow | ModuleStyle::Json => unreachable!("handled above"),
    }
}

struct ResolvedDisplay {
    title: String,
    description: String,
    guidance: Option<String>,
    tags: Vec<String>,
}

fn resolve_display_fields(module: &ScannedModule, use_display: bool) -> ResolvedDisplay {
    let raw_title = module.module_id.clone();
    let raw_desc = module.description.clone();
    let raw_tags = module.tags.clone();
    if !use_display {
        return ResolvedDisplay {
            title: raw_title,
            description: raw_desc,
            guidance: None,
            tags: raw_tags,
        };
    }
    let Some(overlay) = module.display.as_ref().and_then(|v| v.as_object()) else {
        return ResolvedDisplay {
            title: raw_title,
            description: raw_desc,
            guidance: None,
            tags: raw_tags,
        };
    };
    let title = overlay
        .get("alias")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or(raw_title);
    let description = overlay
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or(raw_desc);
    let guidance = overlay
        .get("guidance")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let tags = overlay
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or(raw_tags);
    ResolvedDisplay {
        title,
        description,
        guidance,
        tags,
    }
}

fn render_module_markdown_body(module: &ScannedModule, resolved: &ResolvedDisplay) -> String {
    let mut sections: Vec<String> = Vec::new();
    sections.push(format!("# {}", resolved.title));
    if !resolved.description.is_empty() {
        sections.push(resolved.description.clone());
    }
    if let Some(guidance) = &resolved.guidance {
        sections.push(format!("_{guidance}_"));
    }

    sections.push("## Parameters".to_string());
    let params = render_schema_prose(&module.input_schema, DEFAULT_MAX_DEPTH, 0);
    sections.push(if params.is_empty() {
        "_(no parameters)_".to_string()
    } else {
        params
    });

    sections.push("## Returns".to_string());
    let returns = render_schema_prose(&module.output_schema, DEFAULT_MAX_DEPTH, 0);
    sections.push(if returns.is_empty() {
        "_(no return schema)_".to_string()
    } else {
        returns
    });

    if let Some(table) = render_annotations_table(module.annotations.as_ref()) {
        sections.push("## Behavior".to_string());
        sections.push(table);
    }

    if !module.examples.is_empty() {
        sections.push("## Examples".to_string());
        for (idx, example) in module.examples.iter().enumerate() {
            sections.push(format!("### Example {}", idx + 1));
            sections.push("```json".to_string());
            sections
                .push(serde_json::to_string_pretty(example).unwrap_or_else(|_| "{}".to_string()));
            sections.push("```".to_string());
        }
    }

    if !resolved.tags.is_empty() {
        sections.push("## Tags".to_string());
        let line = resolved
            .tags
            .iter()
            .map(|t| format!("`{t}`"))
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(line);
    }

    let mut body = sections.join("\n\n");
    body.push('\n');
    body
}

/// Render `ModuleAnnotations` as a Markdown fact table.
///
/// Cross-SDK alignment rules (see
/// `apcore-toolkit/docs/features/formatting.md` § Annotations Rendering):
///
/// 1. Emit only fields whose value differs from `ModuleAnnotations::default()`.
/// 2. The `extra` free-form bag is always skipped.
/// 3. Rows are sorted alphabetically by snake_case key (`serde_json::Map`
///    iteration order under default features is already alphabetical via
///    the underlying `BTreeMap`, so this is a property the data structure
///    already gives us).
/// 4. Bool values render as lowercase `true` / `false`; numbers, arrays,
///    and objects use `serde_json::Value::Display` (which is JSON form);
///    string values use their raw content.
///
/// Returns `None` when the resulting table would be empty (every annotation
/// field equals its default), causing the caller to omit the `## Behavior`
/// section entirely.
fn render_annotations_table(annotations: Option<&ModuleAnnotations>) -> Option<String> {
    let value = annotations_to_dict(annotations);
    let obj = value.as_object()?;
    let defaults = default_annotations_dict();
    let mut entries: Vec<(&String, &Value)> = Vec::new();
    for (key, value) in obj.iter() {
        if key == "extra" {
            continue;
        }
        if defaults.get(key) == Some(value) {
            continue;
        }
        entries.push((key, value));
    }
    if entries.is_empty() {
        return None;
    }
    // serde_json::Map iterates in alphabetical key order under default
    // features, so `entries` is already sorted. We do not re-sort.
    let mut rows = vec!["| Flag | Value |".to_string(), "|---|---|".to_string()];
    for (key, value) in entries {
        let rendered = match value {
            Value::String(s) => s.clone(),
            Value::Bool(true) => "true".to_string(),
            Value::Bool(false) => "false".to_string(),
            other => other.to_string(),
        };
        rows.push(format!("| `{key}` | {rendered} |"));
    }
    Some(rows.join("\n"))
}

fn yaml_scalar(text: &str) -> String {
    if text.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quote = text.chars().any(|c| {
        matches!(
            c,
            ':' | '#' | '{' | '}' | '[' | ']' | '\'' | '"' | '\n' | '&' | '*' | '!' | '|' | '>'
        )
    });
    let starts_special = text.starts_with(['-', '?', '%', '@', '`']);
    if !needs_quote && !starts_special {
        return text.to_string();
    }
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Render a sequence of ScannedModule for the chosen surface.
///
/// See `Contract: format_modules` in
/// `apcore-toolkit/docs/features/formatting.md`.
pub fn format_modules(
    modules: &[ScannedModule],
    style: ModuleStyle,
    group_by: Option<GroupBy>,
    display: bool,
) -> FormatOutput {
    if matches!(style, ModuleStyle::Json) {
        return FormatOutput::Values(modules.iter().map(module_to_dict).collect());
    }

    let joiner = match style {
        ModuleStyle::Markdown | ModuleStyle::Skill => "\n\n",
        ModuleStyle::TableRow => "\n",
        ModuleStyle::Json => unreachable!(),
    };

    let render_one = |m: &ScannedModule| -> String {
        match format_module(m, style, display) {
            FormatOutput::Text(s) => s,
            _ => unreachable!("non-text style handled above"),
        }
    };

    let Some(axis) = group_by else {
        let parts: Vec<String> = modules.iter().map(&render_one).collect();
        return FormatOutput::Text(parts.join(joiner));
    };

    let groups = group_modules(modules, axis);
    let mut out: Vec<String> = Vec::new();
    for (group_name, members) in groups {
        let header = match style {
            ModuleStyle::Markdown | ModuleStyle::Skill => format!("## {group_name}"),
            ModuleStyle::TableRow => format!("── {group_name} ──"),
            ModuleStyle::Json => unreachable!(),
        };
        out.push(header);
        for m in members {
            out.push(render_one(m));
        }
    }
    FormatOutput::Text(out.join(joiner))
}

fn group_modules<'a>(
    modules: &'a [ScannedModule],
    axis: GroupBy,
) -> BTreeMap<String, Vec<&'a ScannedModule>> {
    let mut groups: BTreeMap<String, Vec<&'a ScannedModule>> = BTreeMap::new();
    for module in modules {
        match axis {
            GroupBy::Prefix => {
                let prefix = match module.module_id.find('.') {
                    Some(idx) => module.module_id[..idx].to_string(),
                    None => module.module_id.clone(),
                };
                groups.entry(prefix).or_default().push(module);
            }
            GroupBy::Tag => {
                if module.tags.is_empty() {
                    groups
                        .entry("(untagged)".to_string())
                        .or_default()
                        .push(module);
                } else {
                    for tag in &module.tags {
                        groups.entry(tag.clone()).or_default().push(module);
                    }
                }
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::module::{ModuleAnnotations, ModuleExample};
    use serde_json::json;

    fn fixture_module() -> ScannedModule {
        let mut m = ScannedModule::new(
            "users.get_user".into(),
            "Look up a user by id".into(),
            json!({
                "type": "object",
                "properties": {"id": {"type": "integer", "description": "User id"}},
                "required": ["id"],
            }),
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
            }),
            vec!["users".into()],
            "myapp.views:get_user".into(),
        );
        m.annotations = Some(ModuleAnnotations {
            readonly: true,
            cacheable: true,
            ..Default::default()
        });
        m
    }

    // ---------- format_schema ----------

    #[test]
    fn schema_prose_marks_required_and_optional() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "User id"},
                "verbose": {"type": "boolean"},
            },
            "required": ["id"],
        });
        let out = format_schema(&schema, SchemaStyle::Prose, None);
        let s = out.as_str().unwrap();
        assert!(s.contains("`id` (integer, required) — User id"), "got: {s}");
        assert!(s.contains("`verbose` (boolean, optional)"));
    }

    #[test]
    fn schema_table_emits_header_and_yes_no_required() {
        let schema = json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "User id"}},
            "required": ["id"],
        });
        let out = format_schema(&schema, SchemaStyle::Table, None);
        let s = out.as_str().unwrap();
        assert!(s.contains("| Name | Type | Required | Default | Description |"));
        assert!(s.contains("| `id` | integer | yes |  | User id |"));
    }

    #[test]
    fn schema_json_passthrough() {
        let schema = json!({"type": "object"});
        let out = format_schema(&schema, SchemaStyle::Json, None);
        assert_eq!(out.as_value().unwrap(), &schema);
    }

    #[test]
    fn schema_max_depth_collapses_nested() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {
                            "type": "object",
                            "properties": {"deep": {"type": "string"}},
                        },
                    },
                },
            },
        });
        let out = format_schema(&schema, SchemaStyle::Prose, Some(2));
        assert!(out.as_str().unwrap().contains("```json"));
    }

    #[test]
    fn schema_non_object_renders_summary() {
        let out = format_schema(&json!({"type": "string"}), SchemaStyle::Prose, None);
        assert!(out.as_str().unwrap().contains("string"));
    }

    #[test]
    fn schema_empty_prose_returns_empty() {
        let out = format_schema(&json!({}), SchemaStyle::Prose, None);
        assert_eq!(out.as_str().unwrap(), "");
    }

    // ---------- format_module markdown ----------

    #[test]
    fn module_markdown_emits_sections() {
        let out = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        assert!(s.starts_with("# users.get_user"));
        assert!(s.contains("Look up a user by id"));
        assert!(s.contains("## Parameters"));
        assert!(s.contains("## Returns"));
        assert!(s.contains("`id` (integer, required) — User id"));
    }

    #[test]
    fn module_markdown_annotations_fact_table() {
        let out = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("## Behavior"));
        assert!(s.contains("| Flag | Value |"));
        assert!(s.contains("`readonly`"));
        assert!(s.contains("`cacheable`"));
        // destructive matches the default; must not appear.
        assert!(!s.contains("`destructive`"));
    }

    #[test]
    fn module_markdown_annotations_lowercase_bool() {
        let out = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("| `readonly` | true |"));
        assert!(s.contains("| `cacheable` | true |"));
    }

    #[test]
    fn module_markdown_annotations_alphabetical() {
        let out = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        let readonly_idx = s.find("`readonly`").unwrap();
        let cacheable_idx = s.find("`cacheable`").unwrap();
        // 'cacheable' < 'readonly' alphabetically
        assert!(cacheable_idx < readonly_idx);
    }

    #[test]
    fn module_markdown_skips_default_values() {
        let out = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        // pagination_style defaults to "cursor"; must not appear.
        assert!(!s.contains("`pagination_style`"));
    }

    #[test]
    fn module_markdown_omits_behavior_when_all_defaults() {
        let mut m = fixture_module();
        m.annotations = Some(ModuleAnnotations::default());
        let out = format_module(&m, ModuleStyle::Markdown, true);
        assert!(!out.as_str().unwrap().contains("## Behavior"));
    }

    #[test]
    fn module_markdown_omits_behavior_when_annotations_none() {
        let mut m = fixture_module();
        m.annotations = None;
        let out = format_module(&m, ModuleStyle::Markdown, true);
        assert!(!out.as_str().unwrap().contains("## Behavior"));
    }

    #[test]
    fn module_markdown_examples_block() {
        let mut m = fixture_module();
        m.examples = vec![ModuleExample {
            title: "lookup".into(),
            description: None,
            inputs: json!({"id": 1}),
            output: json!({"name": "Ada"}),
        }];
        let out = format_module(&m, ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("## Examples"));
        assert!(s.contains("Ada"));
    }

    #[test]
    fn module_markdown_tags_section() {
        let out = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("## Tags"));
        assert!(s.contains("`users`"));
    }

    // ---------- format_module skill ----------

    #[test]
    fn module_skill_minimal_frontmatter() {
        let out = format_module(&fixture_module(), ModuleStyle::Skill, true);
        let s = out.as_str().unwrap();
        assert!(s.starts_with("---\n"));
        let head = s.split("\n---\n").next().unwrap();
        assert!(head.contains("name: users.get_user"));
        assert!(head.contains("description: "));
        for forbidden in ["allowed-tools", "paths", "when_to_use", "user-invocable"] {
            assert!(
                !s.contains(forbidden),
                "skill output leaked vendor key {forbidden}"
            );
        }
    }

    #[test]
    fn module_skill_body_matches_markdown() {
        let skill = format_module(&fixture_module(), ModuleStyle::Skill, true);
        let markdown = format_module(&fixture_module(), ModuleStyle::Markdown, true);
        let skill_str = skill.as_str().unwrap();
        let body = skill_str.split_once("\n---\n").unwrap().1;
        let body = body.trim_start_matches('\n');
        assert_eq!(body, markdown.as_str().unwrap());
    }

    #[test]
    fn module_skill_quotes_colon_in_description() {
        let mut m = fixture_module();
        m.description = "Get: by id".into();
        let out = format_module(&m, ModuleStyle::Skill, true);
        assert!(out
            .as_str()
            .unwrap()
            .contains("description: \"Get: by id\""));
    }

    // ---------- format_module table-row + json ----------

    #[test]
    fn module_table_row_pipe_separated() {
        let out = format_module(&fixture_module(), ModuleStyle::TableRow, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("`users.get_user`"));
        assert!(s.contains("Look up a user by id"));
        assert!(s.contains("users"));
    }

    #[test]
    fn module_json_passthrough() {
        let out = format_module(&fixture_module(), ModuleStyle::Json, true);
        let v = out.as_value().unwrap();
        assert_eq!(v["module_id"], "users.get_user");
        assert_eq!(v["description"], "Look up a user by id");
    }

    // ---------- display overlay ----------

    #[test]
    fn display_true_uses_overlay() {
        let mut m = fixture_module();
        m.display = Some(json!({
            "alias": "lookup-user",
            "description": "Quickly look someone up.",
            "tags": ["accounts"],
        }));
        let out = format_module(&m, ModuleStyle::Markdown, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("# lookup-user"));
        assert!(s.contains("Quickly look someone up."));
        assert!(s.contains("`accounts`"));
    }

    #[test]
    fn display_false_uses_raw() {
        let mut m = fixture_module();
        m.display = Some(json!({"alias": "lookup-user", "description": "ignored"}));
        let out = format_module(&m, ModuleStyle::Markdown, false);
        let s = out.as_str().unwrap();
        assert!(s.contains("# users.get_user"));
        assert!(s.contains("Look up a user by id"));
        assert!(!s.contains("lookup-user"));
    }

    // ---------- format_modules ----------

    #[test]
    fn modules_ungrouped_concatenates() {
        let mut a = fixture_module();
        let mut b = fixture_module();
        b.module_id = "users.create_user".into();
        b.description = "Create a user".into();
        let out = format_modules(&[a.clone(), b.clone()], ModuleStyle::Markdown, None, true);
        let s = out.as_str().unwrap();
        assert!(s.contains("users.get_user"));
        assert!(s.contains("users.create_user"));
        // Avoid unused mut warning.
        a.module_id.clear();
    }

    #[test]
    fn modules_group_by_tag() {
        let a = fixture_module();
        let mut b = fixture_module();
        b.module_id = "tasks.list".into();
        b.description = "List tasks".into();
        b.tags = vec!["tasks".into()];
        let out = format_modules(&[a, b], ModuleStyle::Markdown, Some(GroupBy::Tag), true);
        let s = out.as_str().unwrap();
        assert!(s.contains("## users"));
        assert!(s.contains("## tasks"));
    }

    #[test]
    fn modules_group_by_prefix() {
        let a = fixture_module();
        let mut b = fixture_module();
        b.module_id = "tasks.list".into();
        b.description = "List tasks".into();
        b.tags = vec![];
        let out = format_modules(&[a, b], ModuleStyle::Markdown, Some(GroupBy::Prefix), true);
        let s = out.as_str().unwrap();
        assert!(s.contains("## users"));
        assert!(s.contains("## tasks"));
    }

    #[test]
    fn modules_json_returns_array_of_dicts() {
        let m = fixture_module();
        let out = format_modules(&[m], ModuleStyle::Json, None, true);
        let arr = out.as_values().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["module_id"], "users.get_user");
    }

    #[test]
    fn modules_untagged_bucket() {
        let mut m = fixture_module();
        m.tags = vec![];
        let out = format_modules(&[m], ModuleStyle::Markdown, Some(GroupBy::Tag), true);
        assert!(out.as_str().unwrap().contains("## (untagged)"));
    }

    // ---------- HEAD/OPTIONS canonical mapping (already correct, smoke test) ----------

    #[test]
    fn scanner_head_options_canonical_mapping() {
        use crate::scanner::infer_annotations_from_method;
        let head = infer_annotations_from_method("HEAD");
        let options = infer_annotations_from_method("OPTIONS");
        assert!(head.readonly);
        assert!(!head.cacheable);
        assert!(options.readonly);
        assert!(!options.cacheable);
    }
}
