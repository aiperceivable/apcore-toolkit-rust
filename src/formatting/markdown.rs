// Generic dict-to-Markdown conversion with depth control and table heuristics.
//
// Provides `to_markdown()` — a best-effort converter for arbitrary JSON values.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Options for Markdown conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkdownOptions {
    /// If provided, only include these top-level keys (order preserved).
    pub fields: Option<Vec<String>>,
    /// Keys to exclude at every nesting level.
    pub exclude: Option<Vec<String>>,
    /// Maximum nesting depth to render. Beyond this, values are shown inline.
    pub max_depth: usize,
    /// When a dict has at least this many keys and all values are scalars,
    /// render as a Markdown table.
    pub table_threshold: usize,
    /// Optional heading prepended to output.
    pub title: Option<String>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            fields: None,
            exclude: None,
            max_depth: 3,
            table_threshold: 5,
            title: None,
        }
    }
}

/// Convert a JSON object to a Markdown string.
///
/// Returns an error if the input is not a JSON object.
pub fn to_markdown(data: &Value, options: &MarkdownOptions) -> Result<String, String> {
    let obj = data.as_object().ok_or_else(|| {
        format!(
            "to_markdown() expects a JSON object, got {}",
            value_type(data)
        )
    })?;

    let filtered = filter_keys(obj, &options.fields, &options.exclude);
    let mut lines: Vec<String> = Vec::new();

    if let Some(title) = &options.title {
        lines.push(format!("# {title}"));
        lines.push(String::new());
    }

    let exclude_set: HashSet<String> = options
        .exclude
        .as_ref()
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default();

    render_dict(
        &filtered,
        &mut lines,
        0,
        0,
        options.max_depth,
        options.table_threshold,
        &exclude_set,
    );

    let mut result = lines.join("\n");
    result = result.trim_end_matches('\n').to_string();
    result.push('\n');
    Ok(result)
}

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn filter_keys(
    obj: &serde_json::Map<String, Value>,
    fields: &Option<Vec<String>>,
    exclude: &Option<Vec<String>>,
) -> Vec<(String, Value)> {
    let mut items: Vec<(String, Value)> = if let Some(f) = fields {
        f.iter()
            .filter_map(|k| obj.get(k).map(|v| (k.clone(), v.clone())))
            .collect()
    } else {
        obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };

    if let Some(ex) = exclude {
        let ex_set: HashSet<&str> = ex.iter().map(|s| s.as_str()).collect();
        items.retain(|(k, _)| !ex_set.contains(k.as_str()));
    }

    items
}

fn is_scalar(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn format_scalar(v: &Value) -> String {
    match v {
        Value::Null => "*N/A*".into(),
        Value::Bool(b) => {
            if *b {
                "Yes".into()
            } else {
                "No".into()
            }
        }
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f == f.trunc() && f.abs() < 1e15 {
                    format!("{}", f as i64)
                } else {
                    format!("{:.4}", f)
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        _ => compact_repr(v, 80),
    }
}

fn escape_pipe(text: &str) -> String {
    text.replace('|', "\\|")
}

fn render_dict(
    items: &[(String, Value)],
    lines: &mut Vec<String>,
    depth: usize,
    abs_depth: usize,
    max_depth: usize,
    table_threshold: usize,
    exclude: &HashSet<String>,
) {
    if items.is_empty() {
        return;
    }

    let filtered: Vec<&(String, Value)> =
        items.iter().filter(|(k, _)| !exclude.contains(k)).collect();

    let all_scalar = filtered.iter().all(|(_, v)| is_scalar(v));

    if all_scalar && filtered.len() >= table_threshold {
        render_table(&filtered, lines);
        return;
    }

    let indent = "  ".repeat(depth);

    for (key, value) in &filtered {
        if is_scalar(value) {
            lines.push(format!("{indent}- **{key}**: {}", format_scalar(value)));
        } else if value.is_object() {
            if abs_depth + 1 >= max_depth {
                lines.push(format!("{indent}- **{key}**: {}", compact_repr(value, 80)));
            } else if depth == 0 {
                let heading_level = (abs_depth + 2).min(6);
                lines.push(String::new());
                lines.push(format!("{} {key}", "#".repeat(heading_level)));
                lines.push(String::new());
                if let Some(obj) = value.as_object() {
                    let sub_items: Vec<(String, Value)> =
                        obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    render_dict(
                        &sub_items,
                        lines,
                        0,
                        abs_depth + 1,
                        max_depth,
                        table_threshold,
                        exclude,
                    );
                }
            } else {
                lines.push(format!("{indent}- **{key}**:"));
                if let Some(obj) = value.as_object() {
                    let sub_items: Vec<(String, Value)> =
                        obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    render_dict(
                        &sub_items,
                        lines,
                        depth + 1,
                        abs_depth + 1,
                        max_depth,
                        table_threshold,
                        exclude,
                    );
                }
            }
        } else if value.is_array() {
            if abs_depth + 1 >= max_depth {
                lines.push(format!("{indent}- **{key}**: {}", compact_repr(value, 80)));
            } else {
                lines.push(format!("{indent}- **{key}**:"));
                if let Some(arr) = value.as_array() {
                    render_list(arr, lines, depth + 1, abs_depth + 1, max_depth, exclude);
                }
            }
        } else {
            lines.push(format!("{indent}- **{key}**: {}", format_scalar(value)));
        }
    }
}

fn render_list(
    items: &[Value],
    lines: &mut Vec<String>,
    depth: usize,
    abs_depth: usize,
    max_depth: usize,
    exclude: &HashSet<String>,
) {
    let indent = "  ".repeat(depth);

    if items.is_empty() {
        lines.push(format!("{indent}- *(empty)*"));
        return;
    }

    // Homogeneous list of scalar-only dicts with uniform keys -> render as table
    if items.len() >= 2
        && items.iter().all(|v| v.is_object())
        && uniform_keys(items)
        && items.iter().all(|v| {
            v.as_object()
                .map(|o| o.values().all(is_scalar))
                .unwrap_or(false)
        })
    {
        render_list_table(items, lines, exclude);
        return;
    }

    for item in items {
        if is_scalar(item) {
            lines.push(format!("{indent}- {}", format_scalar(item)));
        } else if let Some(obj) = item.as_object() {
            if abs_depth >= max_depth {
                lines.push(format!("{indent}- {}", compact_repr(item, 80)));
            } else {
                // Render each dict item inline under a bullet
                let mut first = true;
                for (k, v) in obj {
                    if exclude.contains(k) {
                        continue;
                    }
                    let prefix = if first {
                        first = false;
                        format!("{indent}- ")
                    } else {
                        "  ".repeat(depth + 1)
                    };
                    if is_scalar(v) {
                        lines.push(format!("{prefix}**{k}**: {}", format_scalar(v)));
                    } else {
                        lines.push(format!("{prefix}**{k}**: {}", compact_repr(v, 80)));
                    }
                }
            }
        } else if item.is_array() {
            lines.push(format!("{indent}- {}", compact_repr(item, 80)));
        } else {
            lines.push(format!("{indent}- {}", format_scalar(item)));
        }
    }
}

/// Check if all objects in a list share the same set of keys.
fn uniform_keys(items: &[Value]) -> bool {
    if items.is_empty() {
        return true;
    }
    let first_keys: HashSet<&str> = match items[0].as_object() {
        Some(obj) => obj.keys().map(|k| k.as_str()).collect(),
        None => return false,
    };
    items[1..].iter().all(|v| {
        v.as_object()
            .map(|o| {
                let keys: HashSet<&str> = o.keys().map(|k| k.as_str()).collect();
                keys == first_keys
            })
            .unwrap_or(false)
    })
}

/// Render a list of uniform dicts as a Markdown table.
fn render_list_table(items: &[Value], lines: &mut Vec<String>, exclude: &HashSet<String>) {
    if items.is_empty() {
        return;
    }
    let first_obj = match items[0].as_object() {
        Some(o) => o,
        None => return,
    };
    let keys: Vec<&str> = first_obj
        .keys()
        .map(|k| k.as_str())
        .filter(|k| !exclude.contains(*k))
        .collect();

    lines.push(format!(
        "| {} |",
        keys.iter()
            .map(|k| escape_pipe(k))
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    lines.push(format!(
        "| {} |",
        keys.iter().map(|_| "---").collect::<Vec<_>>().join(" | ")
    ));
    for item in items {
        if let Some(obj) = item.as_object() {
            let row: Vec<String> = keys
                .iter()
                .map(|k| {
                    obj.get(*k)
                        .map(|v| escape_pipe(&format_scalar(v)))
                        .unwrap_or_default()
                })
                .collect();
            lines.push(format!("| {} |", row.join(" | ")));
        }
    }
    lines.push(String::new());
}

fn render_table(items: &[&(String, Value)], lines: &mut Vec<String>) {
    lines.push("| Field | Value |".into());
    lines.push("|-------|-------|".into());
    for (key, value) in items {
        lines.push(format!(
            "| {} | {} |",
            escape_pipe(key),
            escape_pipe(&format_scalar(value))
        ));
    }
    lines.push(String::new());
}

fn compact_repr(value: &Value, max_len: usize) -> String {
    let text = match value {
        Value::Object(obj) => {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{k}: {}", compact_repr(v, 30)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(|v| compact_repr(v, 30)).collect();
            format!("[{}]", parts.join(", "))
        }
        _ => format_scalar(value),
    };

    if text.len() > max_len {
        let truncated: String = text.chars().take(max_len - 3).collect();
        format!("{truncated}...")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_to_markdown_basic() {
        let data = json!({"name": "Alice", "age": 30});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("**name**"));
        assert!(result.contains("Alice"));
        assert!(result.contains("**age**"));
    }

    #[test]
    fn test_to_markdown_with_title() {
        let data = json!({"key": "value"});
        let opts = MarkdownOptions {
            title: Some("My Title".into()),
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        assert!(result.starts_with("# My Title"));
    }

    #[test]
    fn test_to_markdown_non_object() {
        let data = json!("not an object");
        let result = to_markdown(&data, &MarkdownOptions::default());
        assert!(result.is_err());
    }

    #[test]
    fn test_to_markdown_fields_filter() {
        let data = json!({"a": 1, "b": 2, "c": 3});
        let opts = MarkdownOptions {
            fields: Some(vec!["a".into(), "c".into()]),
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        assert!(result.contains("**a**"));
        assert!(result.contains("**c**"));
        assert!(!result.contains("**b**"));
    }

    #[test]
    fn test_to_markdown_exclude() {
        let data = json!({"a": 1, "secret": "hidden", "c": 3});
        let opts = MarkdownOptions {
            exclude: Some(vec!["secret".into()]),
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        assert!(!result.contains("secret"));
    }

    #[test]
    fn test_to_markdown_table_rendering() {
        let data = json!({"a": 1, "b": 2, "c": 3, "d": 4, "e": 5});
        let opts = MarkdownOptions {
            table_threshold: 5,
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        assert!(result.contains("| Field | Value |"));
    }

    #[test]
    fn test_to_markdown_nested_object() {
        let data = json!({"user": {"name": "Alice", "age": 30}});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("## user") || result.contains("**user**"));
    }

    #[test]
    fn test_format_scalar_null() {
        assert_eq!(format_scalar(&Value::Null), "*N/A*");
    }

    #[test]
    fn test_format_scalar_bool() {
        assert_eq!(format_scalar(&json!(true)), "Yes");
        assert_eq!(format_scalar(&json!(false)), "No");
    }

    #[test]
    fn test_to_markdown_empty_dict() {
        let data = json!({});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert_eq!(result, "\n");
    }

    #[test]
    fn test_to_markdown_below_table_threshold() {
        // 3 keys with threshold=5 should render as bullets, not a table
        let data = json!({"a": 1, "b": 2, "c": 3});
        let opts = MarkdownOptions {
            table_threshold: 5,
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        assert!(result.contains("- **a**"));
        assert!(!result.contains("| Field | Value |"));
    }

    #[test]
    fn test_to_markdown_scalar_list() {
        let data = json!({"items": ["alpha", "beta", "gamma"]});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("- alpha"));
        assert!(result.contains("- beta"));
        assert!(result.contains("- gamma"));
    }

    #[test]
    fn test_to_markdown_empty_list() {
        let data = json!({"items": []});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("*(empty)*"));
    }

    #[test]
    fn test_to_markdown_none_renders_na() {
        let data = json!({"value": null});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("*N/A*"));
    }

    #[test]
    fn test_to_markdown_float_precision() {
        // Whole float renders as integer
        let data = json!({"count": 42.0});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("42"));
        assert!(!result.contains("42.0"));

        // Non-whole float renders with 4 decimal places
        let data = json!({"ratio": 1.23456});
        let result = to_markdown(&data, &MarkdownOptions::default()).unwrap();
        assert!(result.contains("1.2346"));
    }

    #[test]
    fn test_to_markdown_pipe_escaped() {
        let data = json!({"a": "x|y", "b": "1", "c": "2", "d": "3", "e": "4"});
        let opts = MarkdownOptions {
            table_threshold: 5,
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        // In the table, pipe characters in values must be escaped
        assert!(result.contains("x\\|y"));
    }

    #[test]
    fn test_to_markdown_max_depth_1() {
        let data = json!({"outer": {"inner": "value"}});
        let opts = MarkdownOptions {
            max_depth: 1,
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        // At max_depth=1 the nested object should be compacted inline
        assert!(result.contains("inner: value"));
        // Should NOT get a sub-heading for 'outer'
        assert!(!result.contains("## outer"));
    }

    #[test]
    fn test_to_markdown_deeply_nested() {
        let data = json!({"l1": {"l2": {"l3": {"l4": "deep"}}}});
        let opts = MarkdownOptions {
            max_depth: 2,
            ..Default::default()
        };
        let result = to_markdown(&data, &opts).unwrap();
        // l2 is at abs_depth=1, l3 would be abs_depth=2 which equals max_depth, so compacted
        assert!(result.contains("l3:"));
        // The deeply nested structure should not be fully expanded
        assert!(!result.contains("## l3"));
    }

    #[test]
    fn test_compact_repr_truncation() {
        let long_value = json!({"key": "a]".repeat(50)});
        let result = compact_repr(&long_value, 20);
        assert!(result.len() <= 20);
        assert!(result.ends_with("..."));
    }
}
