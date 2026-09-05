// TuiViewModel — Tier-1 byte-equivalent module-list view shape.
//
// Lifts the *shape* of a module-list view (columns, rows, filter intent,
// sort intent, color-by-tag rules) into the toolkit, so every downstream
// consumer (apcore-cli-*, future browser dashboards, MCP/A2A surfaces)
// produces identical column sets, identical filter semantics, and
// identical row order for the same `ScannedModule` input. Rendering
// itself stays Tier 2 and is free to differ in pixels.
//
// See apcore-toolkit/docs/features/tui-view-model.md for the full V1
// specification, wire format, and conformance corpus.

use std::collections::{HashMap, HashSet};

use apcore::module::ModuleAnnotations;
use serde_json::{json, Map, Value};

use crate::types::ScannedModule;

/// Top-level view kind (`kind` field of the wire format).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    List,
    Grouped,
}

impl View {
    fn as_str(self) -> &'static str {
        match self {
            View::List => "list",
            View::Grouped => "grouped",
        }
    }
}

/// Column text alignment. `Left` is the wire-format default and is
/// omitted from the encoded `Column` when set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Justify {
    #[default]
    Left,
    Right,
    Center,
}

impl Justify {
    fn as_str(self) -> &'static str {
        match self {
            Justify::Left => "left",
            Justify::Right => "right",
            Justify::Center => "center",
        }
    }
}

/// `Filter.exposure` axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Exposure {
    Exposed,
    Hidden,
    #[default]
    All,
}

impl Exposure {
    fn as_str(self) -> &'static str {
        match self {
            Exposure::Exposed => "exposed",
            Exposure::Hidden => "hidden",
            Exposure::All => "all",
        }
    }
}

/// `Sort.direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Asc => "asc",
            Direction::Desc => "desc",
        }
    }
}

/// Semantic (not visual) cell/tone-rule color classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Positive,
    Negative,
    Warning,
    Info,
}

impl Tone {
    fn as_str(self) -> &'static str {
        match self {
            Tone::Neutral => "neutral",
            Tone::Positive => "positive",
            Tone::Negative => "negative",
            Tone::Warning => "warning",
            Tone::Info => "info",
        }
    }
}

/// Grouping axis for `kind == "grouped"` views.
///
/// Named `ViewGroupBy` (rather than `GroupBy`) to avoid colliding with
/// [`crate::formatting::GroupBy`], the pre-existing group-by axis used by
/// `format_modules`. The two are separate options for separate builders;
/// this type is never part of the encoded wire format (only the computed
/// `Group` list is), so the naming has no cross-SDK/byte-identity impact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewGroupBy {
    Tag,
    Prefix,
}

/// A single table cell (discriminated union by `kind` in the wire format).
///
/// Modelled as a Rust enum (rather than a flat `kind` + optional fields
/// struct, as Python/TypeScript do) so illegal combinations — e.g.
/// `kind: "tags"` carrying a scalar `value` — are unrepresentable. The
/// encoded JSON shape via [`Cell::to_value`] is identical either way.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Text { value: String, tone: Option<Tone> },
    Tags { values: Vec<String> },
    Badge { value: String, tone: Option<Tone> },
    Symbol { value: String, tone: Option<Tone> },
}

impl Cell {
    pub fn to_value(&self) -> Value {
        let mut d = Map::new();
        match self {
            Cell::Text { value, tone } => {
                d.insert("kind".to_string(), Value::String("text".to_string()));
                d.insert("value".to_string(), Value::String(value.clone()));
                insert_tone(&mut d, *tone);
            }
            Cell::Tags { values } => {
                d.insert("kind".to_string(), Value::String("tags".to_string()));
                d.insert(
                    "values".to_string(),
                    Value::Array(values.iter().cloned().map(Value::String).collect()),
                );
            }
            Cell::Badge { value, tone } => {
                d.insert("kind".to_string(), Value::String("badge".to_string()));
                d.insert("value".to_string(), Value::String(value.clone()));
                insert_tone(&mut d, *tone);
            }
            Cell::Symbol { value, tone } => {
                d.insert("kind".to_string(), Value::String("symbol".to_string()));
                d.insert("value".to_string(), Value::String(value.clone()));
                insert_tone(&mut d, *tone);
            }
        }
        Value::Object(d)
    }
}

fn insert_tone(d: &mut Map<String, Value>, tone: Option<Tone>) {
    if let Some(t) = tone {
        d.insert("tone".to_string(), Value::String(t.as_str().to_string()));
    }
}

/// A view-model column: render order and `Row.cells` index lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub key: String,
    pub label: String,
    pub justify: Justify,
    pub tone_by: Option<String>,
}

impl Column {
    pub fn to_value(&self) -> Value {
        let mut d = Map::new();
        d.insert("key".to_string(), Value::String(self.key.clone()));
        d.insert("label".to_string(), Value::String(self.label.clone()));
        if self.justify != Justify::Left {
            d.insert(
                "justify".to_string(),
                Value::String(self.justify.as_str().to_string()),
            );
        }
        if let Some(tone_by) = &self.tone_by {
            d.insert("tone_by".to_string(), Value::String(tone_by.clone()));
        }
        Value::Object(d)
    }
}

/// A view-model row. `cells[i]` corresponds to `columns[i]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub tags: Vec<String>,
}

impl Row {
    pub fn to_value(&self) -> Value {
        let mut d = Map::new();
        d.insert(
            "cells".to_string(),
            Value::Array(self.cells.iter().map(Cell::to_value).collect()),
        );
        if !self.tags.is_empty() {
            d.insert(
                "tags".to_string(),
                Value::Array(self.tags.iter().cloned().map(Value::String).collect()),
            );
        }
        Value::Object(d)
    }
}

/// Annotates which sort the toolkit (or the caller) applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    pub key: String,
    pub direction: Direction,
}

impl Sort {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            direction: Direction::Asc,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "key": self.key,
            "direction": self.direction.as_str(),
        })
    }
}

/// Annotates which filter the toolkit applied. All fields required in the
/// encoded form — see `Canonical JSON Encoding` in the spec.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub tags: Vec<String>,
    pub search: String,
    pub annotations: Vec<String>,
    pub exposure: Exposure,
    pub deprecated: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            search: String::new(),
            annotations: Vec::new(),
            exposure: Exposure::All,
            deprecated: true,
        }
    }
}

impl Filter {
    pub fn to_value(&self) -> Value {
        json!({
            "tags": self.tags,
            "search": self.search,
            "annotations": self.annotations,
            "exposure": self.exposure.as_str(),
            "deprecated": self.deprecated,
        })
    }
}

/// First-match-wins rule mapping a tag to a semantic tone.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneRule {
    pub value: String,
    pub tone: Tone,
}

impl ToneRule {
    pub fn to_value(&self) -> Value {
        json!({
            "match": { "kind": "tag_equals", "value": self.value },
            "tone": self.tone.as_str(),
        })
    }
}

/// A named, ordered set of [`ToneRule`].
#[derive(Debug, Clone, PartialEq)]
pub struct TonePalette {
    pub name: String,
    pub rules: Vec<ToneRule>,
}

impl TonePalette {
    pub fn to_value(&self) -> Value {
        json!({
            "name": self.name,
            "rules": self.rules.iter().map(ToneRule::to_value).collect::<Vec<_>>(),
        })
    }
}

/// A named group of row indices, present only when `kind == "grouped"`.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub label: String,
    pub row_indices: Vec<usize>,
}

impl Group {
    pub fn to_value(&self) -> Value {
        json!({
            "label": self.label,
            "row_indices": self.row_indices,
        })
    }
}

/// The V1 `TuiViewModel` wire-format envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct TuiViewModel {
    pub kind: View,
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    pub schema_version: i64,
    pub title: Option<String>,
    pub groups: Option<Vec<Group>>,
    pub sort: Option<Sort>,
    pub filter: Option<Filter>,
    pub tone_palettes: Option<Vec<TonePalette>>,
}

impl TuiViewModel {
    /// Build the declaration-order `serde_json::Value` for this view model.
    ///
    /// Key order mirrors the Python/TypeScript encoders exactly:
    /// `schema_version, kind, [title], columns, rows, [groups], [sort],
    /// [filter], [tone_palettes]`.
    pub fn to_value(&self) -> Value {
        let mut d = Map::new();
        d.insert(
            "schema_version".to_string(),
            Value::Number(self.schema_version.into()),
        );
        d.insert(
            "kind".to_string(),
            Value::String(self.kind.as_str().to_string()),
        );
        if let Some(title) = &self.title {
            d.insert("title".to_string(), Value::String(title.clone()));
        }
        d.insert(
            "columns".to_string(),
            Value::Array(self.columns.iter().map(Column::to_value).collect()),
        );
        d.insert(
            "rows".to_string(),
            Value::Array(self.rows.iter().map(Row::to_value).collect()),
        );
        if let Some(groups) = &self.groups {
            d.insert(
                "groups".to_string(),
                Value::Array(groups.iter().map(Group::to_value).collect()),
            );
        }
        if let Some(sort) = &self.sort {
            d.insert("sort".to_string(), sort.to_value());
        }
        if let Some(filter) = &self.filter {
            d.insert("filter".to_string(), filter.to_value());
        }
        // Unlike `groups`/`sort`/`filter` (omitted purely on `None`-ness),
        // `tone_palettes` is also omitted when present-but-empty, mirroring
        // Python's `if self.tone_palettes:` (truthy/non-empty check) and
        // TypeScript's explicit `.length > 0` check. `modules_to_view_model`
        // itself never produces `Some(vec![])` (empty input is normalized to
        // `None` before construction), but `TuiViewModel`'s fields are all
        // `pub`, so a hand-built value passed directly to `format_view_model`
        // can reach this state.
        if let Some(tone_palettes) = &self.tone_palettes {
            if !tone_palettes.is_empty() {
                d.insert(
                    "tone_palettes".to_string(),
                    Value::Array(tone_palettes.iter().map(TonePalette::to_value).collect()),
                );
            }
        }
        Value::Object(d)
    }
}

/// Canonical, byte-identical compact JSON encoding of `vm`.
///
/// See `apcore-toolkit/docs/features/tui-view-model.md` § Canonical JSON
/// Encoding: declaration-order keys, optional fields omitted (never
/// `null`), lowercase booleans, no floating point, no whitespace between
/// tokens.
pub fn format_view_model(vm: &TuiViewModel) -> String {
    serde_json::to_string(&vm.to_value()).unwrap_or_default()
}

/// Options for [`modules_to_view_model`].
///
/// No built-in default column set: `columns` defaults to empty — there is
/// no conventional layout applied automatically (see conformance fixture
/// `view_model_001_empty_list`). Callers wanting the conventional
/// `["module_id", "description", "tags"]` layout pass it explicitly.
#[derive(Debug, Clone)]
pub struct ModulesToViewModelOptions {
    pub view: View,
    pub columns: Vec<String>,
    pub title: Option<String>,
    pub filter: Option<Filter>,
    pub sort: Option<Sort>,
    pub group_by: Option<ViewGroupBy>,
    pub tone_palettes: Vec<TonePalette>,
    pub display: bool,
}

impl Default for ModulesToViewModelOptions {
    fn default() -> Self {
        Self {
            view: View::List,
            columns: Vec::new(),
            title: None,
            filter: None,
            sort: None,
            group_by: None,
            tone_palettes: Vec::new(),
            display: true,
        }
    }
}

fn column_label(key: &str) -> String {
    match key {
        "module_id" => "ID".to_string(),
        "alias" => "Alias".to_string(),
        "description" => "Description".to_string(),
        "tags" => "Tags".to_string(),
        other => other.to_string(),
    }
}

/// Coerce a `display` overlay field to a display string, mirroring the
/// truthy-coercion the Python (`if alias: return str(alias)`) and
/// TypeScript SDKs apply to a loosely-typed JSON value.
///
/// `display` is documented and typed as `Option<serde_json::Value>` in all
/// three SDKs, so a hand-authored binding (e.g. `display: {alias: 007}`)
/// may carry a non-string JSON value. Strings and numbers/bools are
/// reasonably stringifiable and are coerced directly (matching what
/// `str()`/`String()` would produce); `Null`, `Array`, and `Object` are not
/// reasonably stringifiable as a display value and fall through to the
/// caller's fallback, matching Python's `if alias:` treating `None`/`[]`/
/// `{}` as falsy. An empty string is likewise falsy in Python and falls
/// through here too.
fn display_field_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn resolve_alias(module: &ScannedModule, use_display: bool) -> String {
    if use_display {
        if let Some(display) = &module.display {
            if let Some(alias) = display.get("alias").and_then(display_field_as_string) {
                return alias;
            }
        }
    }
    module.module_id.clone()
}

fn resolve_description(module: &ScannedModule, use_display: bool) -> String {
    if use_display {
        if let Some(display) = &module.display {
            if let Some(description) = display.get("description").and_then(display_field_as_string)
            {
                return description;
            }
        }
    }
    module.description.clone()
}

fn cell_for(column_key: &str, module: &ScannedModule) -> Cell {
    match column_key {
        "module_id" => Cell::Text {
            value: module.module_id.clone(),
            tone: None,
        },
        "tags" => Cell::Tags {
            values: module.tags.clone(),
        },
        // Unrecognised/custom column key: empty text cell rather than a
        // panic, so a caller-declared column with no toolkit-known source
        // still yields a well-formed row.
        _ => Cell::Text {
            value: String::new(),
            tone: None,
        },
    }
}

/// `Filter.annotations` names one of `ModuleAnnotations`'s boolean flag
/// fields, which must be `true`. Reflection isn't available in Rust, so
/// this is a manual name → field lookup covering every boolean flag on
/// `ModuleAnnotations`. An unrecognised name is treated as `false`
/// (never satisfied), matching Python's `getattr(ann, name, False)`
/// fallback for a nonexistent attribute.
/// Look up one of `ModuleAnnotations`' boolean flag fields by its
/// snake_case name, as used by `Filter.annotations`.
///
/// Deliberately restricted to the 9 fields the spec calls "`ModuleAnnotations`
/// flag fields" — `cache_ttl`, `cache_key_fields`, `pagination_style`, and
/// `extra` are not boolean and have no defined truthiness contract, so a
/// filter naming one of them matches nothing (`false`) rather than
/// reflection-coercing an arbitrary field to a bool. Python (`getattr` +
/// `bool()`) and TypeScript (a raw property lookup) would coerce such a
/// field instead; this is a known, narrow divergence on out-of-contract
/// filter input, not a defect — see the cross-SDK audit that flagged it.
fn annotation_flag(annotations: Option<&ModuleAnnotations>, name: &str) -> bool {
    let Some(ann) = annotations else {
        return false;
    };
    match name {
        "readonly" => ann.readonly,
        "destructive" => ann.destructive,
        "idempotent" => ann.idempotent,
        "requires_approval" => ann.requires_approval,
        "open_world" => ann.open_world,
        "streaming" => ann.streaming,
        "cacheable" => ann.cacheable,
        "paginated" => ann.paginated,
        "discoverable" => ann.discoverable,
        _ => false,
    }
}

fn passes_filter(module: &ScannedModule, flt: Option<&Filter>, description: &str) -> bool {
    let Some(flt) = flt else {
        return true;
    };
    let module_tags: HashSet<&str> = module.tags.iter().map(String::as_str).collect();
    if !flt.tags.is_empty() && !flt.tags.iter().all(|t| module_tags.contains(t.as_str())) {
        return false;
    }
    if !flt.search.is_empty() {
        let haystack = format!("{} {}", module.module_id, description).to_lowercase();
        if !haystack.contains(&flt.search.to_lowercase()) {
            return false;
        }
    }
    let annotations = module.annotations.as_ref();
    for annotation_name in &flt.annotations {
        if !annotation_flag(annotations, annotation_name) {
            return false;
        }
    }
    // `discoverable` (ModuleAnnotations, default true) is the shipped
    // signal for "appears in enumeration surfaces" — hidden means not
    // discoverable.
    let is_hidden = !annotations.map(|a| a.discoverable).unwrap_or(true);
    if flt.exposure == Exposure::Exposed && is_hidden {
        return false;
    }
    if flt.exposure == Exposure::Hidden && !is_hidden {
        return false;
    }
    // ModuleAnnotations has no first-class `deprecated` field; the toolkit
    // convention (matching OpenAPIScanner) is `annotations.extra["deprecated"]`.
    let is_deprecated = annotations
        .and_then(|a| a.extra.get("deprecated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !flt.deprecated && is_deprecated {
        return false;
    }
    true
}

fn sort_key(column_key: &str, module: &ScannedModule, alias: &str, description: &str) -> String {
    match column_key {
        "alias" => alias.to_string(),
        "description" => description.to_string(),
        _ => module.module_id.clone(),
    }
}

const SORTABLE_KEYS: [&str; 3] = ["module_id", "alias", "description"];

/// Build a byte-equivalent [`TuiViewModel`] from scanned modules.
///
/// See `Contract: modules_to_view_model` and the wire-format schema in
/// `apcore-toolkit/docs/features/tui-view-model.md`.
///
/// Sort/filter execution model: filtering by `tags` / `search` /
/// `annotations` / `exposure` / `deprecated` always executes here.
/// Sorting by `module_id` / `alias` / `description` executes here; any
/// other `sort.key` (e.g. usage-based `calls` / `errors` / `latency`) is
/// honoured verbatim in the incoming `modules` order — the caller is
/// responsible for pre-sorting those.
pub fn modules_to_view_model(
    modules: &[ScannedModule],
    options: &ModulesToViewModelOptions,
) -> TuiViewModel {
    // V1 convention: with no explicit per-column wiring in the public API,
    // the first supplied palette (if any) is referenced by the "tags"
    // column's `tone_by` — the only column shape a `tag_equals` rule can
    // meaningfully colour. Per-value tone resolution (which tag chip gets
    // which colour) is a Tier-2 renderer concern, not computed here.
    let tags_palette = options.tone_palettes.first();

    let column_objs: Vec<Column> = options
        .columns
        .iter()
        .map(|key| {
            let tone_by = if key == "tags" {
                tags_palette.map(|p| p.name.clone())
            } else {
                None
            };
            Column {
                key: key.clone(),
                label: column_label(key),
                justify: Justify::Left,
                tone_by,
            }
        })
        .collect();

    let mut resolved: Vec<(&ScannedModule, String, String)> = Vec::new();
    for module in modules {
        let alias = resolve_alias(module, options.display);
        let description = resolve_description(module, options.display);
        if !passes_filter(module, options.filter.as_ref(), &description) {
            continue;
        }
        resolved.push((module, alias, description));
    }

    if let Some(sort) = &options.sort {
        if SORTABLE_KEYS.contains(&sort.key.as_str()) {
            let reverse = sort.direction == Direction::Desc;
            resolved.sort_by(|a, b| {
                let ka = sort_key(&sort.key, a.0, &a.1, &a.2);
                let kb = sort_key(&sort.key, b.0, &b.1, &b.2);
                if reverse {
                    kb.cmp(&ka)
                } else {
                    ka.cmp(&kb)
                }
            });
        }
    }

    let rows: Vec<Row> = resolved
        .iter()
        .map(|(module, alias, description)| {
            let cells: Vec<Cell> = column_objs
                .iter()
                .map(|column| match column.key.as_str() {
                    "alias" => Cell::Text {
                        value: alias.clone(),
                        tone: None,
                    },
                    "description" => Cell::Text {
                        value: description.clone(),
                        tone: None,
                    },
                    other => cell_for(other, module),
                })
                .collect();
            Row {
                cells,
                tags: module.tags.clone(),
            }
        })
        .collect();

    let groups = if options.view == View::Grouped {
        Some(build_groups(&resolved, options.group_by))
    } else {
        None
    };

    TuiViewModel {
        kind: options.view,
        columns: column_objs,
        rows,
        schema_version: 1,
        title: options.title.clone(),
        groups,
        sort: options.sort.clone(),
        filter: options.filter.clone(),
        tone_palettes: if options.tone_palettes.is_empty() {
            None
        } else {
            Some(options.tone_palettes.clone())
        },
    }
}

fn build_groups(
    resolved: &[(&ScannedModule, String, String)],
    group_by: Option<ViewGroupBy>,
) -> Vec<Group> {
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (idx, (module, _alias, _description)) in resolved.iter().enumerate() {
        let labels: Vec<String> = match group_by {
            Some(ViewGroupBy::Tag) => {
                if module.tags.is_empty() {
                    vec!["(untagged)".to_string()]
                } else {
                    module.tags.clone()
                }
            }
            Some(ViewGroupBy::Prefix) => {
                vec![module
                    .module_id
                    .split('.')
                    .next()
                    .unwrap_or(&module.module_id)
                    .to_string()]
            }
            None => vec!["(all)".to_string()],
        };
        for label in labels {
            if !buckets.contains_key(&label) {
                order.push(label.clone());
                buckets.insert(label.clone(), Vec::new());
            }
            buckets.get_mut(&label).expect("just inserted").push(idx);
        }
    }
    order
        .into_iter()
        .map(|label| {
            let row_indices = buckets.remove(&label).unwrap_or_default();
            Group { label, row_indices }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_module(id: &str, description: &str, tags: &[&str]) -> ScannedModule {
        ScannedModule::new(
            id.into(),
            description.into(),
            json!({}),
            json!({}),
            tags.iter().map(|t| t.to_string()).collect(),
            "fixture:noop".into(),
        )
    }

    #[test]
    fn test_empty_list() {
        let vm = modules_to_view_model(&[], &ModulesToViewModelOptions::default());
        assert_eq!(
            format_view_model(&vm),
            r#"{"schema_version":1,"kind":"list","columns":[],"rows":[]}"#
        );
    }

    #[test]
    fn test_basic_columns() {
        let modules = vec![make_module("a.one", "A one", &["x"])];
        let options = ModulesToViewModelOptions {
            columns: vec!["module_id".into(), "description".into(), "tags".into()],
            ..Default::default()
        };
        let vm = modules_to_view_model(&modules, &options);
        assert_eq!(vm.columns.len(), 3);
        assert_eq!(vm.columns[0].label, "ID");
        assert_eq!(vm.rows.len(), 1);
    }

    #[test]
    fn test_unknown_column_key_yields_empty_text_cell() {
        let modules = vec![make_module("a.one", "A one", &[])];
        let options = ModulesToViewModelOptions {
            columns: vec!["custom_field".into()],
            ..Default::default()
        };
        let vm = modules_to_view_model(&modules, &options);
        assert_eq!(vm.columns[0].label, "custom_field");
        match &vm.rows[0].cells[0] {
            Cell::Text { value, .. } => assert_eq!(value, ""),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    #[test]
    fn test_sort_desc_stable_for_ties() {
        // Two modules share the same description; reverse=true must not
        // disturb their relative order (matches Python's stable-sort +
        // reverse=True semantics).
        let modules = vec![
            make_module("a.first", "Same", &[]),
            make_module("a.second", "Same", &[]),
        ];
        let options = ModulesToViewModelOptions {
            columns: vec!["module_id".into()],
            sort: Some(Sort {
                key: "description".into(),
                direction: Direction::Desc,
            }),
            ..Default::default()
        };
        let vm = modules_to_view_model(&modules, &options);
        match &vm.rows[0].cells[0] {
            Cell::Text { value, .. } => assert_eq!(value, "a.first"),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
        match &vm.rows[1].cells[0] {
            Cell::Text { value, .. } => assert_eq!(value, "a.second"),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    // Regression tests: `display.alias`/`display.description` are typed as
    // loosely-typed JSON (`Option<serde_json::Value>`) in all three SDKs, so
    // a hand-authored binding like `display: {alias: 007}` or
    // `{alias: true}` is valid input. Python/TypeScript coerce ANY truthy
    // value to a string (`str(alias)`/`String(alias)`); `resolve_alias` /
    // `resolve_description` previously required the value already be a
    // JSON string (`.and_then(Value::as_str)`), silently falling back to
    // the raw `module_id`/`description` for a number or bool value instead
    // of stringifying it — a cross-SDK behavioural divergence on the same
    // binding document.
    #[test]
    fn test_resolve_alias_coerces_numeric_display_value() {
        let mut module = make_module("svc.thing", "A thing", &[]);
        module.display = Some(json!({"alias": 42}));
        let options = ModulesToViewModelOptions {
            columns: vec!["alias".into()],
            display: true,
            ..Default::default()
        };
        let vm = modules_to_view_model(&[module], &options);
        match &vm.rows[0].cells[0] {
            Cell::Text { value, .. } => assert_eq!(
                value, "42",
                "numeric display.alias must be stringified, not fall back to module_id"
            ),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_alias_coerces_boolean_display_value() {
        let mut module = make_module("svc.thing", "A thing", &[]);
        module.display = Some(json!({"alias": true}));
        let options = ModulesToViewModelOptions {
            columns: vec!["alias".into()],
            display: true,
            ..Default::default()
        };
        let vm = modules_to_view_model(&[module], &options);
        match &vm.rows[0].cells[0] {
            Cell::Text { value, .. } => assert_eq!(
                value, "true",
                "boolean display.alias must be stringified, not fall back to module_id"
            ),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_description_coerces_numeric_display_value() {
        let mut module = make_module("svc.thing", "A thing", &[]);
        module.display = Some(json!({"description": 7}));
        let options = ModulesToViewModelOptions {
            columns: vec!["description".into()],
            display: true,
            ..Default::default()
        };
        let vm = modules_to_view_model(&[module], &options);
        match &vm.rows[0].cells[0] {
            Cell::Text { value, .. } => assert_eq!(
                value, "7",
                "numeric display.description must be stringified, not fall back to raw description"
            ),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_alias_still_falls_back_for_null_array_object() {
        // Null/Array/Object are not reasonably stringifiable as a display
        // value and Python's `if alias:` treats None/[]/{} as falsy too —
        // these must keep falling through to the module_id fallback.
        for bad_value in [json!(null), json!([]), json!({})] {
            let mut module = make_module("svc.thing", "A thing", &[]);
            module.display = Some(json!({"alias": bad_value}));
            let options = ModulesToViewModelOptions {
                columns: vec!["alias".into()],
                display: true,
                ..Default::default()
            };
            let vm = modules_to_view_model(&[module], &options);
            match &vm.rows[0].cells[0] {
                Cell::Text { value, .. } => assert_eq!(
                    value, "svc.thing",
                    "non-stringifiable display.alias {bad_value:?} must fall back to module_id"
                ),
                other => panic!("expected Cell::Text, got {other:?}"),
            }
        }
    }

    // Regression test: Python (`if self.tone_palettes:`) and TypeScript
    // (`.length > 0`) both omit `tone_palettes` from the encoded view model
    // when the list is present-but-empty — unlike `groups`/`sort`/`filter`,
    // which are omitted purely on `None`-ness. `modules_to_view_model`
    // itself never produces `Some(vec![])`, but `TuiViewModel`'s fields are
    // all `pub`, so a hand-built value passed directly to
    // `format_view_model` can reach this state.
    #[test]
    fn test_format_view_model_omits_empty_tone_palettes() {
        let vm = TuiViewModel {
            kind: View::List,
            columns: vec![],
            rows: vec![],
            schema_version: 1,
            title: None,
            groups: None,
            sort: None,
            filter: None,
            tone_palettes: Some(vec![]),
        };
        let encoded = format_view_model(&vm);
        assert!(
            !encoded.contains("tone_palettes"),
            "present-but-empty tone_palettes must be omitted, matching Python/TypeScript; got: {encoded}"
        );
    }
}
