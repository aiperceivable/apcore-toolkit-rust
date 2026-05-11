// Byte-equivalent tabular data formatters: CSV and JSONL.
//
// Cross-SDK byte-identity contract: every SDK (Python / TypeScript / Rust)
// emits identical bytes for the same input. Consumers (apcore-cli, apcore-mcp,
// apcore-a2a, downstream CLIs) MUST delegate to these formatters rather than
// reimplementing.
//
// See apcore-toolkit/docs/features/formatting.md § Tabular Formats.

use serde_json::{Map, Value};

const BOM: char = '\u{FEFF}';

/// Render rows as RFC 4180 CSV.
///
/// Header columns are the union of keys across all rows, preserved in
/// insertion order from first occurrence. Rows missing a key emit an empty
/// cell. Non-scalar values are serialized as canonical JSON inside the cell.
/// Cells containing `,`, `"`, `\n`, or `\r` are quote-wrapped with embedded
/// `"` doubled. Line terminator is CRLF.
pub fn format_csv(rows: &[Map<String, Value>], bom: bool) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let mut keys: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        for k in row.keys() {
            if seen.insert(k.clone()) {
                keys.push(k.clone());
            }
        }
    }

    let mut lines: Vec<String> = Vec::with_capacity(rows.len() + 1);
    lines.push(csv_join(
        &keys.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    ));
    for row in rows {
        let cells: Vec<String> = keys.iter().map(|k| csv_cell(row.get(k))).collect();
        lines.push(csv_join(
            &cells.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ));
    }

    let mut body = lines.join("\r\n");
    body.push_str("\r\n");
    if bom {
        let mut out = String::with_capacity(body.len() + 3);
        out.push(BOM);
        out.push_str(&body);
        out
    } else {
        body
    }
}

/// Render rows as JSON Lines. Each row is canonical compact JSON; LF
/// terminator; no trailing blank line.
pub fn format_jsonl(rows: &[Map<String, Value>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for row in rows {
        out.push_str(&canonical_json(&Value::Object(row.clone())));
        out.push('\n');
    }
    out
}

fn csv_join(cells: &[&str]) -> String {
    cells
        .iter()
        .map(|c| csv_escape(c))
        .collect::<Vec<_>>()
        .join(",")
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

fn csv_cell(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(true)) => "true".to_string(),
        Some(Value::Bool(false)) => "false".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => canonical_number(n),
        Some(v @ (Value::Array(_) | Value::Object(_))) => canonical_json(v),
    }
}

/// Canonical compact JSON aligned with JS `JSON.stringify`: no whitespace
/// between tokens, insertion-order preserved (via `serde_json` `preserve_order`
/// feature), unicode preserved, whole-number floats render as plain integers.
fn canonical_json(value: &Value) -> String {
    let canonicalized = canonicalize_value(value);
    serde_json::to_string(&canonicalized).unwrap_or_default()
}

fn canonicalize_value(value: &Value) -> Value {
    match value {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    if f == f.trunc() && f.abs() < (i64::MAX as f64) {
                        // Whole-number float → int, matching JS canonical form.
                        Value::Number(serde_json::Number::from(f as i64))
                    } else {
                        // Preserve original number representation for fractional values.
                        Value::Number(n.clone())
                    }
                } else {
                    Value::Null
                }
            } else {
                Value::Number(n.clone())
            }
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_value).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), canonicalize_value(v));
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn canonical_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(f) = n.as_f64() {
        if !f.is_finite() {
            return String::new();
        }
        if f == f.trunc() && f.abs() < (i64::MAX as f64) {
            return (f as i64).to_string();
        }
        return f.to_string();
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("row helper expects an object"),
        }
    }

    #[test]
    fn empty_csv() {
        assert_eq!(format_csv(&[], false), "");
    }

    #[test]
    fn single_row_csv() {
        let rows = vec![row(json!({"a": 1, "b": 2}))];
        assert_eq!(format_csv(&rows, false), "a,b\r\n1,2\r\n");
    }

    #[test]
    fn heterogeneous_keys_csv() {
        let rows = vec![row(json!({"a": 1})), row(json!({"a": 2, "b": 3}))];
        assert_eq!(format_csv(&rows, false), "a,b\r\n1,\r\n2,3\r\n");
    }

    #[test]
    fn nested_object_csv() {
        let rows = vec![row(json!({"schema": {"type": "object"}}))];
        let out = format_csv(&rows, false);
        assert!(out.contains("\"{\"\"type\"\":\"\"object\"\"}\""));
        assert!(!out.contains("'"));
    }

    #[test]
    fn rfc4180_escaping() {
        assert_eq!(
            format_csv(&[row(json!({"a": "x,y"}))], false),
            "a\r\n\"x,y\"\r\n"
        );
        assert_eq!(
            format_csv(&[row(json!({"a": "she said \"hi\""}))], false),
            "a\r\n\"she said \"\"hi\"\"\"\r\n"
        );
    }

    #[test]
    fn scalar_types_csv() {
        let rows = vec![row(json!({
            "n": null,
            "b": true,
            "f": false,
            "i": 42,
            "fw": 1.0,
            "ff": 1.5,
        }))];
        assert_eq!(
            format_csv(&rows, false),
            "n,b,f,i,fw,ff\r\n,true,false,42,1,1.5\r\n"
        );
    }

    #[test]
    fn bom_option() {
        let rows = vec![row(json!({"a": 1}))];
        assert!(format_csv(&rows, true).starts_with('\u{FEFF}'));
        assert!(!format_csv(&rows, false).starts_with('\u{FEFF}'));
    }

    #[test]
    fn empty_jsonl() {
        assert_eq!(format_jsonl(&[]), "");
    }

    #[test]
    fn jsonl_lf_no_trailing_blank() {
        let rows = vec![row(json!({"a": 1})), row(json!({"b": 2}))];
        assert_eq!(format_jsonl(&rows), "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn jsonl_canonical_float() {
        let rows = vec![row(json!({"fw": 1.0, "ff": 1.5}))];
        assert_eq!(format_jsonl(&rows), "{\"fw\":1,\"ff\":1.5}\n");
    }
}
