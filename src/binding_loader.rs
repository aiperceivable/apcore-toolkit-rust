// BindingLoader — parse `.binding.yaml` files back into `ScannedModule`.
//
// Inverse of `output::yaml_writer::YAMLWriter`. Unlike apcore's own
// `BindingLoader` (which imports the target and registers a runtime module),
// this loader is pure data: it parses YAML into `ScannedModule` objects for
// validation, merging, diffing, or round-trip workflows. No code is loaded.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use apcore::module::{ModuleAnnotations, ModuleExample};
use serde_json::Value;
use thiserror::Error;
use tracing::warn;

use crate::types::ScannedModule;

const SUPPORTED_SPEC_VERSIONS: &[&str] = &["1.0"];

/// Keys whose presence in a metadata dict is unsafe for cross-runtime
/// round-trip — they correspond to JS prototype-pollution sinks. They are
/// stripped at parse time so a malicious or malformed binding YAML cannot
/// carry an attacker-controlled `__proto__` / `constructor` / `prototype`
/// entry into downstream consumers (matches the TypeScript loader's
/// `PROTO_DENY` guard in `src/binding-parser.ts`).
const FORBIDDEN_METADATA_KEYS: &[&str] = &["__proto__", "constructor", "prototype"];

/// Maximum size of a single `.binding.yaml` file (16 MiB).
///
/// A binding file is a structured YAML document, not a data store.
/// Files larger than this are almost certainly pathological and would
/// cause the full content to be loaded into memory twice (raw bytes +
/// serde_yaml_ng::Value + serde_json::Value).
const MAX_BINDING_FILE_SIZE: u64 = 16 * 1024 * 1024;

/// Maximum number of `.binding.yaml` files loaded from a single directory.
///
/// Prevents a maliciously large directory from causing unbounded memory
/// consumption in `load_data` callers that accumulate results.
const MAX_BINDING_FILES_PER_DIR: usize = 10_000;

/// Default value of the `bindings.pattern` configuration key
/// (`apcore` `schemas/defaults.schema.json`), used when a caller passes
/// `None` to [`BindingLoader::load_with_pattern`].
pub const DEFAULT_BINDING_PATTERN: &str = "*.binding.yaml";

/// Errors produced by [`BindingLoader`].
#[derive(Debug, Error)]
pub enum BindingLoadError {
    /// The path does not exist or cannot be stat'd.
    #[error("path does not exist: {path}")]
    PathNotFound { path: String },

    /// A binding file exceeds the maximum allowed size.
    #[error("binding file {path} is too large ({size} bytes > {max} byte limit)")]
    FileTooLarge { path: String, size: u64, max: u64 },

    /// The directory contains more binding files than the per-directory limit.
    #[error("directory {path} contains more than {max} binding files")]
    TooManyFiles { path: String, max: usize },

    /// Failure reading a binding file from disk.
    #[error("failed to read {path}: {source}")]
    FileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The file content is not valid YAML.
    #[error("failed to parse YAML in {path}: {source}")]
    YamlParse {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    /// A binding entry is missing or has an invalid value for one or more
    /// required fields. Covers three cases: absent key, explicit `null`, and
    /// wrong-type scalar (e.g. `module_id: 42`, `target: true`). All three
    /// are treated as "required field not supplied" rather than silently
    /// coerced to empty strings or zero values downstream.
    #[error("missing or invalid required fields {missing_fields:?} (file={}, module_id={})",
        .path.as_deref().unwrap_or("<inline>"),
        .module_id.as_deref().unwrap_or("<unknown>"))]
    MissingFields {
        path: Option<String>,
        module_id: Option<String>,
        missing_fields: Vec<String>,
    },

    /// The document structure is invalid (e.g. top-level is not a mapping,
    /// or `bindings` is not a list).
    #[error("invalid binding structure in {}: {reason}", .path.as_deref().unwrap_or("<inline>"))]
    InvalidStructure {
        path: Option<String>,
        reason: String,
    },
}

/// Match a binding file **name** against a `bindings.pattern` glob.
///
/// This is the normative matcher shared by the Python, TypeScript, and Rust
/// SDKs — see `docs/features/binding-loader.md#normative-matching-algorithm`
/// and the shared fixture `conformance/fixtures/binding_pattern.json`.
///
/// Exactly two metacharacters are recognised:
///
/// | Token | Meaning |
/// |---|---|
/// | `*` | Zero or more characters, including `.` |
/// | `?` | Exactly one character |
/// | anything else | A literal, including `[`, `]`, `{`, `}`, `!`, `^`, `-` |
///
/// Character classes and brace expansion are deliberately **not** supported;
/// matching is case-sensitive on every platform, applies no Unicode
/// normalization, and does not exclude leading-dot names.
///
/// The implementation is the standard two-pointer glob match with
/// single-star backtracking, which bounds the work at
/// `O(pattern.len() * name.len())` — the naive recursive matcher is
/// exponential on inputs such as `*a*a*a*a*b`, and `pattern` arrives from
/// configuration.
///
/// Comparison is over Unicode **code points**, not bytes: both inputs are
/// collected into `char` vectors first, so a single astral character is
/// consumed by exactly one `?`.
pub fn match_binding_pattern(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let nam: Vec<char> = name.chars().collect();

    // Cursors into `pat` and `nam`.
    let mut p: usize = 0;
    let mut n: usize = 0;
    // Position of the most recent `*`, and where to resume `nam` when
    // backtracking into it.
    let mut star: Option<usize> = None;
    let mut mark: usize = 0;

    while n < nam.len() {
        if p < pat.len() && pat[p] == '?' {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == '*' {
            // Consume zero characters for now; widen on backtrack.
            star = Some(p);
            mark = n;
            p += 1;
        } else if p < pat.len() && pat[p] == nam[n] {
            p += 1;
            n += 1;
        } else if let Some(star_pos) = star {
            // Let the most recent `*` eat one more character.
            p = star_pos + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }

    // Trailing stars may match nothing.
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }

    p == pat.len()
}

/// Loads `.binding.yaml` files into [`ScannedModule`] objects.
///
/// # Usage
///
/// ```ignore
/// let loader = BindingLoader;
/// let modules = loader.load(Path::new("bindings/"), false, false)?;
/// let strict = loader.load(Path::new("foo.binding.yaml"), true, false)?;
///
/// // Honour a configured `bindings.pattern` — the caller resolves, the
/// // loader matches.
/// let cli = loader.load_with_pattern(
///     Path::new("bindings/"),
///     false,
///     true,
///     Some("api-*.cli.yaml"),
/// )?;
/// ```
///
/// In loose mode (`strict=false`, default), only `module_id` and `target`
/// are required; missing optional fields fall back to defaults.
///
/// In strict mode (`strict=true`), `input_schema` and `output_schema` are
/// additionally required.
#[derive(Debug, Default)]
pub struct BindingLoader;

impl BindingLoader {
    /// Create a new BindingLoader.
    pub fn new() -> Self {
        Self
    }

    /// Load one file or every [`DEFAULT_BINDING_PATTERN`] match in a directory.
    ///
    /// Delegates to [`BindingLoader::load_with_pattern`] with `pattern = None`.
    /// Kept at three arguments so existing callers keep compiling; this is the
    /// same two-tier shape `apcore` uses for `load_binding_dir` /
    /// `load_binding_dir_with_config`.
    pub fn load(
        &self,
        path: &Path,
        strict: bool,
        recursive: bool,
    ) -> Result<Vec<ScannedModule>, BindingLoadError> {
        self.load_with_pattern(path, strict, recursive, None)
    }

    /// Load one file, or every file in a directory whose **name** matches
    /// `pattern`.
    ///
    /// `pattern` is the resolved value of `apcore`'s `bindings.pattern`
    /// configuration key; `None` selects [`DEFAULT_BINDING_PATTERN`]. The
    /// loader takes a value and does not read a `Config` itself —
    /// environment > file > default resolution belongs to the caller, which
    /// is the layer that holds the `Config`. See
    /// [`match_binding_pattern`] for the supported syntax.
    ///
    /// `pattern` and `recursive` are orthogonal: `recursive` governs **which
    /// directories are traversed**, `pattern` governs **which file names
    /// match** at whatever depth traversal reached. A caller never writes a
    /// `**/` prefix and the loader never synthesises one. When `path` names a
    /// file, `pattern` is ignored for *matching* — exactly like `recursive` —
    /// but it is still *validated*: validation is a pure precondition on the
    /// argument, not a step of the directory walk.
    ///
    /// Directories are never candidates at any depth: a directory whose name
    /// matches `pattern` is skipped, not selected and then failed on at read
    /// time. The file-type check **follows symlinks** — a symlink to a regular
    /// file is selected, a broken symlink is skipped like any non-file.
    /// Traversal still does not descend into a symlinked *directory*, so such
    /// a link is neither selected nor walked.
    ///
    /// Matched files are sorted lexicographically by path before parsing, the
    /// directory load stays all-or-nothing, and the safety caps still apply to
    /// the matched set. `pattern` narrows *which* files are read; it changes
    /// nothing about how they are read.
    ///
    /// # Pattern validity
    ///
    /// Every string is a valid pattern; this never returns an error for a
    /// syntactic reason. `/`, `\`, `[`, `]`, `{` and `}` are literals, so an
    /// odd pattern simply matches no file name. Matches apcore's Algorithm
    /// A25 requirement 2 (PROTOCOL_SPEC §9.2.3) and §5.12.6 clause 6.
    ///
    /// # OS error handling vs. TypeScript
    ///
    /// Note: unlike the TypeScript implementation which warns and continues on OS
    /// permission errors (EACCES/EPERM), this implementation propagates any IO error
    /// as `BindingLoadError::FileRead` and aborts the entire load. This is the
    /// fail-fast behavior.
    pub fn load_with_pattern(
        &self,
        path: &Path,
        strict: bool,
        recursive: bool,
        pattern: Option<&str>,
    ) -> Result<Vec<ScannedModule>, BindingLoadError> {
        let pattern: &str = pattern.unwrap_or(DEFAULT_BINDING_PATTERN);
        let files: Vec<PathBuf> = if path.is_file() {
            vec![path.to_path_buf()]
        } else if path.is_dir() {
            let mut entries: Vec<PathBuf> = if recursive {
                // Surface per-entry traversal failures (permission denied,
                // broken symlink, I/O errors) rather than silently dropping
                // them — matches the non-recursive branch's policy so a
                // caller switching `recursive=false` → `true` gets a
                // consistent error contract.
                let mut flat: Vec<PathBuf> = Vec::new();
                for entry_result in WalkDir::new(path).max_depth(64).follow_links(false) {
                    let entry = entry_result.map_err(|e| {
                        let io_err = e
                            .into_io_error()
                            .unwrap_or_else(|| std::io::Error::other("walkdir traversal error"));
                        BindingLoadError::FileRead {
                            path: path.display().to_string(),
                            source: io_err,
                        }
                    })?;
                    // Directories are never candidates. The file-type
                    // check follows symlinks — `Path::is_file` stats the
                    // target — so a symlink to a regular file is selected,
                    // while a symlink to a directory and a broken symlink
                    // (whose stat fails) are not. `DirEntry::file_type()`
                    // would report on the link itself and silently drop every
                    // symlinked binding file.
                    //
                    // Following file symlinks is not following directory
                    // symlinks: `follow_links(false)` above stays, so
                    // traversal never descends into a symlinked directory,
                    // which is where cycles and tree-escape live.
                    if match_binding_pattern(pattern, &entry.file_name().to_string_lossy())
                        && entry.path().is_file()
                    {
                        flat.push(entry.into_path());
                    }
                }
                flat
            } else {
                let read_dir = fs::read_dir(path).map_err(|e| BindingLoadError::FileRead {
                    path: path.display().to_string(),
                    source: e,
                })?;
                let mut flat: Vec<PathBuf> = Vec::new();
                for entry_result in read_dir {
                    match entry_result {
                        Ok(entry) => {
                            let p = entry.path();
                            let name_matches = p
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| match_binding_pattern(pattern, n));
                            // Directories are never candidates: a directory
                            // named `x.binding.yaml` is skipped rather than
                            // selected and then failed on at read time. Same
                            // following-stat rule as the recursive branch —
                            // test the target, not the link.
                            if name_matches && p.is_file() {
                                flat.push(p);
                            }
                        }
                        Err(e) => {
                            // Surface per-entry failures rather than silently
                            // discarding them; a permission error on a single
                            // file should not make the directory load partial.
                            return Err(BindingLoadError::FileRead {
                                path: path.display().to_string(),
                                source: e,
                            });
                        }
                    }
                }
                flat
            };
            entries.sort();
            entries
        } else {
            return Err(BindingLoadError::PathNotFound {
                path: path.display().to_string(),
            });
        };

        if files.len() > MAX_BINDING_FILES_PER_DIR {
            return Err(BindingLoadError::TooManyFiles {
                path: path.display().to_string(),
                max: MAX_BINDING_FILES_PER_DIR,
            });
        }

        let mut modules: Vec<ScannedModule> = Vec::new();
        for f in files {
            let file_size = fs::metadata(&f)
                .map_err(|e| BindingLoadError::FileRead {
                    path: f.display().to_string(),
                    source: e,
                })?
                .len();
            if file_size > MAX_BINDING_FILE_SIZE {
                return Err(BindingLoadError::FileTooLarge {
                    path: f.display().to_string(),
                    size: file_size,
                    max: MAX_BINDING_FILE_SIZE,
                });
            }
            let content = fs::read_to_string(&f).map_err(|e| BindingLoadError::FileRead {
                path: f.display().to_string(),
                source: e,
            })?;
            let raw: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(&content).map_err(|e| BindingLoadError::YamlParse {
                    path: f.display().to_string(),
                    source: e,
                })?;
            if raw.is_null() {
                warn!("BindingLoader: {} is empty, skipping", f.display());
                continue;
            }
            let json_value =
                serde_json::to_value(raw).map_err(|e| BindingLoadError::InvalidStructure {
                    path: Some(f.display().to_string()),
                    reason: format!("YAML → JSON conversion failed: {e}"),
                })?;
            modules.extend(self.parse_document(
                &json_value,
                Some(&f.display().to_string()),
                strict,
            )?);
        }
        Ok(modules)
    }

    /// Parse a pre-loaded binding JSON value (`{"bindings": [...]}`).
    pub fn load_data(
        &self,
        data: &Value,
        strict: bool,
    ) -> Result<Vec<ScannedModule>, BindingLoadError> {
        self.parse_document(data, None, strict)
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn parse_document(
        &self,
        raw: &Value,
        file_path: Option<&str>,
        strict: bool,
    ) -> Result<Vec<ScannedModule>, BindingLoadError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| BindingLoadError::InvalidStructure {
                path: file_path.map(String::from),
                reason: "top-level binding document must be a mapping".into(),
            })?;

        Self::check_spec_version(obj.get("spec_version"), file_path);

        let bindings = obj
            .get("bindings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| BindingLoadError::InvalidStructure {
                path: file_path.map(String::from),
                reason: "'bindings' key missing or not a list".into(),
            })?;

        let mut modules: Vec<ScannedModule> = Vec::with_capacity(bindings.len());
        for entry in bindings {
            let entry_obj =
                entry
                    .as_object()
                    .ok_or_else(|| BindingLoadError::InvalidStructure {
                        path: file_path.map(String::from),
                        reason: "binding entry must be a mapping".into(),
                    })?;
            modules.push(Self::parse_entry(entry_obj, file_path, strict)?);
        }
        Ok(modules)
    }

    fn check_spec_version(spec_version: Option<&Value>, file_path: Option<&str>) {
        let where_str = file_path.unwrap_or("<inline>");
        match spec_version {
            None | Some(Value::Null) => {
                warn!(
                    "BindingLoader: {} missing 'spec_version'; defaulting to '1.0'.",
                    where_str
                );
            }
            Some(v) => {
                let as_str = v.as_str();
                if !as_str.is_some_and(|s| SUPPORTED_SPEC_VERSIONS.contains(&s)) {
                    warn!(
                        "BindingLoader: {} has spec_version={} newer than supported {:?}; proceeding best-effort.",
                        where_str, v, SUPPORTED_SPEC_VERSIONS
                    );
                }
            }
        }
    }

    fn parse_entry(
        entry: &serde_json::Map<String, Value>,
        file_path: Option<&str>,
        strict: bool,
    ) -> Result<ScannedModule, BindingLoadError> {
        let required: &[&str] = if strict {
            &["module_id", "target", "input_schema", "output_schema"]
        } else {
            &["module_id", "target"]
        };

        // A required field is "missing or invalid" when absent, null, or of
        // the wrong type. Previously only None/Null was rejected, so
        // `module_id: 42` or `target: true` would silently coerce to an
        // empty string downstream and corrupt the registered module.
        let missing: Vec<String> = required
            .iter()
            .filter(|f| match entry.get(**f) {
                None | Some(Value::Null) => true,
                Some(v) => match **f {
                    // Schemas must be objects.
                    "input_schema" | "output_schema" => !v.is_object(),
                    // Identifiers must be non-empty strings.
                    _ => v.as_str().is_none_or(|s| s.is_empty()),
                },
            })
            .map(|f| (*f).to_string())
            .collect();
        if !missing.is_empty() {
            return Err(BindingLoadError::MissingFields {
                path: file_path.map(String::from),
                module_id: entry
                    .get("module_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                missing_fields: missing,
            });
        }

        let module_id = entry
            .get("module_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let target = entry
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let description = entry
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let version = entry
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("1.0.0")
            .to_string();

        let documentation = entry
            .get("documentation")
            .and_then(|v| v.as_str())
            .map(String::from);

        let suggested_alias = entry
            .get("suggested_alias")
            .and_then(|v| v.as_str())
            .map(String::from);

        let input_schema = entry
            .get("input_schema")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let output_schema = entry
            .get("output_schema")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let tags: Vec<String> = entry
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let warnings: Vec<String> = entry
            .get("warnings")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let metadata: HashMap<String, Value> = entry
            .get("metadata")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| {
                        if FORBIDDEN_METADATA_KEYS.contains(&k.as_str()) {
                            warn!(
                                module_id = %module_id,
                                key = %k,
                                "BindingLoader: dropping forbidden metadata key (prototype-pollution guard)"
                            );
                            None
                        } else {
                            Some((k.clone(), v.clone()))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let display = Self::parse_display(entry.get("display"), &module_id);

        let annotations = Self::parse_annotations(entry.get("annotations"), &module_id);
        let examples = Self::parse_examples(entry.get("examples"), &module_id);

        Ok(ScannedModule {
            module_id,
            description,
            input_schema,
            output_schema,
            tags,
            target,
            version,
            annotations,
            documentation,
            suggested_alias,
            examples,
            metadata,
            display,
            warnings,
        })
    }

    fn parse_display(value: Option<&Value>, module_id: &str) -> Option<Value> {
        let v = value?;
        if v.is_null() {
            return None;
        }
        if !v.is_object() {
            warn!(
                "BindingLoader: display for module {} is not an object; ignoring",
                module_id
            );
            return None;
        }
        Some(v.clone())
    }

    fn parse_annotations(value: Option<&Value>, module_id: &str) -> Option<ModuleAnnotations> {
        let v = value?;
        if v.is_null() {
            return None;
        }
        if !v.is_object() {
            warn!(
                "BindingLoader: annotations for module {} is not a dict; treating as None",
                module_id
            );
            return None;
        }
        match serde_json::from_value::<ModuleAnnotations>(v.clone()) {
            Ok(ann) => Some(ann),
            Err(e) => {
                warn!(
                    "BindingLoader: failed to parse annotations for module {}: {}; treating as None",
                    module_id, e
                );
                None
            }
        }
    }

    fn parse_examples(value: Option<&Value>, module_id: &str) -> Vec<ModuleExample> {
        let Some(v) = value else {
            return Vec::new();
        };
        if v.is_null() {
            return Vec::new();
        }
        let Some(arr) = v.as_array() else {
            warn!(
                "BindingLoader: examples for module {} is not a list; ignoring",
                module_id
            );
            return Vec::new();
        };
        let mut result = Vec::with_capacity(arr.len());
        for (i, ex) in arr.iter().enumerate() {
            if !ex.is_object() {
                warn!(
                    "BindingLoader: examples[{}] of module {} is not a dict; ignoring",
                    i, module_id
                );
                continue;
            }
            match serde_json::from_value::<ModuleExample>(ex.clone()) {
                Ok(parsed) => result.push(parsed),
                Err(e) => warn!(
                    "BindingLoader: examples[{}] of module {} malformed: {}; ignoring",
                    i, module_id, e
                ),
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn minimal_entry() -> Value {
        json!({"module_id": "x.y", "target": "pkg:func"})
    }

    fn full_entry() -> Value {
        json!({
            "module_id": "users.get_user",
            "target": "myapp.views:get_user",
            "description": "Get a user",
            "documentation": "Returns a user by ID.",
            "tags": ["users", "get"],
            "version": "2.0.0",
            "annotations": {"readonly": true, "cacheable": true, "cache_ttl": 60},
            "examples": [
                {"title": "happy", "inputs": {"id": 1}, "output": {"name": "alice"}}
            ],
            "metadata": {"http_method": "GET"},
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "display": {"mcp": {"alias": "users_get"}, "alias": "users.get"},
            "suggested_alias": "users.get.alt",
            "warnings": ["stale"]
        })
    }

    #[test]
    fn test_loose_minimum_entry() {
        let loader = BindingLoader::new();
        let modules = loader
            .load_data(&json!({"bindings": [minimal_entry()]}), false)
            .unwrap();
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.module_id, "x.y");
        assert_eq!(m.target, "pkg:func");
        assert_eq!(m.description, "");
        assert_eq!(m.version, "1.0.0");
        assert!(m.annotations.is_none());
        assert!(m.display.is_none());
        assert!(m.tags.is_empty());
        assert_eq!(m.input_schema, json!({}));
        assert_eq!(m.output_schema, json!({}));
    }

    // D11-5b regression: `__proto__` / `constructor` / `prototype` keys in
    // binding-YAML metadata must be dropped at parse time so they cannot
    // propagate to JS-side consumers (cross-language prototype-pollution
    // guard). Mirrors the TypeScript loader's `PROTO_DENY` and the Python
    // loader's `_FORBIDDEN_METADATA_KEYS`.
    #[test]
    fn test_metadata_filters_proto_pollution_keys() {
        let loader = BindingLoader::new();
        let entry = json!({
            "module_id": "x.y",
            "target": "pkg:func",
            "metadata": {
                "__proto__": {"polluted": true},
                "constructor": "evil",
                "prototype": ["bad"],
                "safe_key": "kept",
            }
        });
        let modules = loader
            .load_data(&json!({"bindings": [entry]}), false)
            .unwrap();
        assert_eq!(modules.len(), 1);
        let metadata = &modules[0].metadata;
        assert!(!metadata.contains_key("__proto__"));
        assert!(!metadata.contains_key("constructor"));
        assert!(!metadata.contains_key("prototype"));
        assert_eq!(metadata.get("safe_key"), Some(&json!("kept")));
    }

    #[test]
    fn test_strict_requires_input_schema() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(&json!({"bindings": [minimal_entry()]}), true)
            .unwrap_err();
        match err {
            BindingLoadError::MissingFields {
                missing_fields,
                module_id,
                ..
            } => {
                assert!(missing_fields.contains(&"input_schema".to_string()));
                assert!(missing_fields.contains(&"output_schema".to_string()));
                assert_eq!(module_id.as_deref(), Some("x.y"));
            }
            _ => panic!("expected MissingFields, got {err:?}"),
        }
    }

    #[test]
    fn test_strict_accepts_when_schemas_present() {
        let loader = BindingLoader::new();
        let entry = json!({
            "module_id": "x.y",
            "target": "pkg:func",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}
        });
        let modules = loader
            .load_data(&json!({"bindings": [entry]}), true)
            .unwrap();
        assert_eq!(modules.len(), 1);
    }

    #[test]
    fn test_missing_module_id_always_fails() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(&json!({"bindings": [{"target": "p:f"}]}), false)
            .unwrap_err();
        assert!(matches!(
            err,
            BindingLoadError::MissingFields { ref missing_fields, .. }
                if missing_fields.contains(&"module_id".to_string())
        ));
    }

    #[test]
    fn test_missing_target_always_fails() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(&json!({"bindings": [{"module_id": "x"}]}), false)
            .unwrap_err();
        assert!(matches!(
            err,
            BindingLoadError::MissingFields { ref missing_fields, .. }
                if missing_fields.contains(&"target".to_string())
        ));
    }

    #[test]
    fn test_missing_bindings_key() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(&json!({"spec_version": "1.0"}), false)
            .unwrap_err();
        assert!(matches!(
            err,
            BindingLoadError::InvalidStructure { ref reason, .. } if reason.contains("bindings")
        ));
    }

    #[test]
    fn test_top_level_not_mapping() {
        let loader = BindingLoader::new();
        let err = loader.load_data(&json!(["a", "b"]), false).unwrap_err();
        assert!(matches!(
            err,
            BindingLoadError::InvalidStructure { ref reason, .. } if reason.contains("mapping")
        ));
    }

    #[test]
    fn test_entry_not_a_mapping() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(&json!({"bindings": ["scalar"]}), false)
            .unwrap_err();
        assert!(matches!(
            err,
            BindingLoadError::InvalidStructure { ref reason, .. } if reason.contains("mapping")
        ));
    }

    #[test]
    fn test_annotations_parsed() {
        let loader = BindingLoader::new();
        let m = &loader
            .load_data(&json!({"bindings": [full_entry()]}), false)
            .unwrap()[0];
        let ann = m.annotations.as_ref().expect("annotations should parse");
        assert!(ann.readonly);
        assert!(ann.cacheable);
        assert_eq!(ann.cache_ttl, 60);
    }

    #[test]
    fn test_annotations_wrong_type_treated_as_none() {
        let loader = BindingLoader::new();
        let m = &loader
            .load_data(
                &json!({"bindings": [{"module_id": "x", "target": "p:f", "annotations": "readonly"}]}),
                false,
            )
            .unwrap()[0];
        assert!(m.annotations.is_none());
    }

    #[test]
    fn test_missing_fields_error_message_is_readable() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(&json!({"bindings": [{"module_id": "x"}]}), false)
            .unwrap_err();
        let msg = err.to_string();
        // No raw debug-format wrappers leak into the user-facing message.
        assert!(!msg.contains("Some("), "got: {msg}");
        assert!(!msg.contains("None"), "got: {msg}");
        assert!(msg.contains("x"), "module_id missing from message: {msg}");
        assert!(msg.contains("target"), "missing field not listed: {msg}");
    }

    #[test]
    fn test_display_wrong_type_dropped() {
        // Malformed display (non-object) is dropped. We can't easily capture
        // tracing warnings without a subscriber, but we assert the drop occurs.
        let loader = BindingLoader::new();
        let m = &loader
            .load_data(
                &json!({"bindings": [{"module_id": "x", "target": "p:f", "display": "not-a-dict"}]}),
                false,
            )
            .unwrap()[0];
        assert!(m.display.is_none());
    }

    #[test]
    fn test_display_null_dropped() {
        let loader = BindingLoader::new();
        let m = &loader
            .load_data(
                &json!({"bindings": [{"module_id": "x", "target": "p:f", "display": null}]}),
                false,
            )
            .unwrap()[0];
        assert!(m.display.is_none());
    }

    #[test]
    fn test_display_preserved() {
        let loader = BindingLoader::new();
        let m = &loader
            .load_data(&json!({"bindings": [full_entry()]}), false)
            .unwrap()[0];
        assert_eq!(
            m.display.as_ref().unwrap(),
            &json!({"mcp": {"alias": "users_get"}, "alias": "users.get"})
        );
    }

    #[test]
    fn test_examples_parsed() {
        let loader = BindingLoader::new();
        let m = &loader
            .load_data(&json!({"bindings": [full_entry()]}), false)
            .unwrap()[0];
        assert_eq!(m.examples.len(), 1);
        assert_eq!(m.examples[0].title, "happy");
    }

    #[test]
    fn test_file_too_large_error_variant() {
        // Verify the FileTooLarge variant can be constructed and displays correctly.
        // The actual 16 MiB threshold is impractical to trigger in a unit test
        // (we'd need to write a 16 MiB file), but this test confirms the error
        // type is wired up and the display message is sensible.
        let err = BindingLoadError::FileTooLarge {
            path: "/bindings/huge.binding.yaml".to_string(),
            size: MAX_BINDING_FILE_SIZE + 1,
            max: MAX_BINDING_FILE_SIZE,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("too large"),
            "message should mention size: {msg}"
        );
        assert!(
            msg.contains("huge.binding.yaml"),
            "message should mention path: {msg}"
        );
    }

    #[test]
    fn test_load_single_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("one.binding.yaml");
        let doc = json!({"spec_version": "1.0", "bindings": [full_entry()]});
        fs::write(&file, serde_yaml_ng::to_string(&doc).unwrap()).unwrap();
        let modules = BindingLoader::new().load(&file, false, false).unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].module_id, "users.get_user");
    }

    #[test]
    fn test_load_directory_sorted() {
        let dir = TempDir::new().unwrap();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let f = dir.path().join(format!("{name}.binding.yaml"));
            let doc = json!({
                "spec_version": "1.0",
                "bindings": [{"module_id": name, "target": format!("pkg:f{i}")}]
            });
            fs::write(&f, serde_yaml_ng::to_string(&doc).unwrap()).unwrap();
        }
        fs::write(dir.path().join("unrelated.yaml"), "irrelevant: true").unwrap();

        let modules = BindingLoader::new().load(dir.path(), false, false).unwrap();
        let ids: Vec<&str> = modules.iter().map(|m| m.module_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_nonexistent_path() {
        let dir = TempDir::new().unwrap();
        let err = BindingLoader::new()
            .load(&dir.path().join("nope"), false, false)
            .unwrap_err();
        assert!(matches!(err, BindingLoadError::PathNotFound { .. }));
    }

    #[test]
    fn test_malformed_yaml() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("bad.binding.yaml");
        fs::write(&f, "::: not yaml :::\n  - [").unwrap();
        let err = BindingLoader::new().load(&f, false, false).unwrap_err();
        assert!(matches!(err, BindingLoadError::YamlParse { .. }));
    }

    #[test]
    fn test_empty_file_skipped() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("empty.binding.yaml");
        fs::write(&f, "").unwrap();
        let modules = BindingLoader::new().load(&f, false, false).unwrap();
        assert!(modules.is_empty());
    }

    #[test]
    fn test_round_trip_with_yaml_writer() {
        use crate::output::yaml_writer::YAMLWriter;

        let mut original = ScannedModule::new(
            "round.trip".into(),
            "Round-trip test".into(),
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            json!({"type": "object"}),
            vec!["demo".into()],
            "demo.app:handler".into(),
        );
        original.version = "1.2.3".into();
        original.annotations = Some(ModuleAnnotations {
            readonly: true,
            streaming: true,
            cache_ttl: 30,
            ..Default::default()
        });
        original.documentation = Some("Docs here".into());
        original.metadata.insert("http_method".into(), json!("GET"));
        original.display = Some(json!({"mcp": {"alias": "rt"}, "alias": "round-trip"}));

        let dir = TempDir::new().unwrap();
        YAMLWriter
            .write(
                &[original.clone()],
                dir.path().to_str().unwrap(),
                false,
                false,
                None,
            )
            .unwrap();

        let loaded = BindingLoader::new().load(dir.path(), false, false).unwrap();
        assert_eq!(loaded.len(), 1);
        let m = &loaded[0];
        assert_eq!(m.module_id, original.module_id);
        assert_eq!(m.target, original.target);
        assert_eq!(m.description, original.description);
        assert_eq!(m.documentation, original.documentation);
        assert_eq!(m.tags, original.tags);
        assert_eq!(m.version, original.version);
        assert_eq!(m.input_schema, original.input_schema);
        assert_eq!(m.output_schema, original.output_schema);
        assert_eq!(m.metadata, original.metadata);
        assert_eq!(m.display, original.display);
        let ann = m.annotations.as_ref().unwrap();
        assert!(ann.readonly);
        assert!(ann.streaming);
        assert_eq!(ann.cache_ttl, 30);
    }

    // ---- Wrong-type scalar rejection (D1-1 regression guard) ----

    #[test]
    fn test_wrong_type_module_id_integer_rejected() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(
                &json!({"bindings": [{"module_id": 42, "target": "p:f"}]}),
                false,
            )
            .unwrap_err();
        assert!(
            matches!(
                &err,
                BindingLoadError::MissingFields { missing_fields, .. }
                    if missing_fields.iter().any(|f| f == "module_id")
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn test_wrong_type_target_bool_rejected() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(
                &json!({"bindings": [{"module_id": "x", "target": true}]}),
                false,
            )
            .unwrap_err();
        assert!(
            matches!(
                &err,
                BindingLoadError::MissingFields { missing_fields, .. }
                    if missing_fields.iter().any(|f| f == "target")
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn test_empty_string_module_id_rejected() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(
                &json!({"bindings": [{"module_id": "", "target": "p:f"}]}),
                false,
            )
            .unwrap_err();
        assert!(
            matches!(
                &err,
                BindingLoadError::MissingFields { missing_fields, .. }
                    if missing_fields.iter().any(|f| f == "module_id")
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn test_strict_wrong_type_input_schema_rejected() {
        let loader = BindingLoader::new();
        let err = loader
            .load_data(
                &json!({"bindings": [{
                    "module_id": "x",
                    "target": "p:f",
                    "input_schema": 42,
                    "output_schema": {"type": "object"}
                }]}),
                true,
            )
            .unwrap_err();
        assert!(
            matches!(
                &err,
                BindingLoadError::MissingFields { missing_fields, .. }
                    if missing_fields.iter().any(|f| f == "input_schema")
            ),
            "got: {err:?}"
        );
    }

    // ---- Recursive WalkDir error propagation (D1-2 regression guard) ----

    #[test]
    #[cfg(unix)]
    fn test_recursive_load_surfaces_walkdir_errors() {
        use std::os::unix::fs::PermissionsExt;

        // Running as root bypasses UNIX permissions and makes this test
        // a no-op. Skip in that case rather than produce a misleading pass.
        let is_root = libc_geteuid() == 0;
        if is_root {
            return;
        }

        let dir = TempDir::new().unwrap();
        let unreadable = dir.path().join("unreadable");
        fs::create_dir(&unreadable).unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

        let result = BindingLoader::new().load(dir.path(), false, true);

        // Restore permissions so TempDir::drop can clean up.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).ok();

        assert!(
            matches!(result, Err(BindingLoadError::FileRead { .. })),
            "recursive load should propagate per-entry I/O errors, got: {result:?}",
        );
    }

    #[cfg(unix)]
    fn libc_geteuid() -> u32 {
        // Avoid a libc dev-dep solely for this test — inline the syscall.
        extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: `geteuid` is a stateless C function that takes no args
        // and returns the effective UID.
        unsafe { geteuid() }
    }

    #[test]
    fn test_load_recursive_finds_nested_files() {
        let dir = TempDir::new().unwrap();
        let subdir = dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();

        // File in root dir
        let doc_root = json!({"spec_version": "1.0", "bindings": [{"module_id": "root.mod", "target": "pkg:f0"}]});
        fs::write(
            dir.path().join("root.binding.yaml"),
            serde_yaml_ng::to_string(&doc_root).unwrap(),
        )
        .unwrap();

        // File in subdir
        let doc_sub = json!({"spec_version": "1.0", "bindings": [{"module_id": "sub.mod", "target": "pkg:f1"}]});
        fs::write(
            subdir.join("sub.binding.yaml"),
            serde_yaml_ng::to_string(&doc_sub).unwrap(),
        )
        .unwrap();

        // Non-recursive: only root
        let flat = BindingLoader::new().load(dir.path(), false, false).unwrap();
        let flat_ids: Vec<&str> = flat.iter().map(|m| m.module_id.as_str()).collect();
        assert_eq!(flat_ids, vec!["root.mod"]);

        // Recursive: both
        let recursive = BindingLoader::new().load(dir.path(), false, true).unwrap();
        let mut rec_ids: Vec<&str> = recursive.iter().map(|m| m.module_id.as_str()).collect();
        rec_ids.sort();
        assert_eq!(rec_ids, vec!["root.mod", "sub.mod"]);
    }

    // ---- Pattern matching (docs/features/binding-loader.md#pattern-matching) ----

    #[test]
    fn test_match_binding_pattern_star_semantics() {
        // `*` crosses `.`, matches zero characters, and does not exclude
        // leading-dot names.
        assert!(match_binding_pattern(
            "*.binding.yaml",
            "users.binding.yaml"
        ));
        assert!(match_binding_pattern(
            "*.binding.yaml",
            "a.b.c.binding.yaml"
        ));
        assert!(match_binding_pattern("*.binding.yaml", ".binding.yaml"));
        assert!(match_binding_pattern(
            "*.binding.yaml",
            ".hidden.binding.yaml"
        ));
        assert!(match_binding_pattern("*", "anything.at.all"));
        assert!(!match_binding_pattern(
            "*.binding.yaml",
            "users.binding.yml"
        ));
    }

    #[test]
    fn test_match_binding_pattern_multiple_stars_backtrack() {
        assert!(match_binding_pattern(
            "*-*.binding.yaml",
            "api-v1.binding.yaml"
        ));
        assert!(!match_binding_pattern(
            "*-*.binding.yaml",
            "apiv1.binding.yaml"
        ));
        // `**` without a separator is just `*`; it is not globstar.
        assert!(match_binding_pattern(
            "**.binding.yaml",
            "users.binding.yaml"
        ));
    }

    #[test]
    fn test_match_binding_pattern_question_is_exactly_one_char() {
        assert!(match_binding_pattern("v?.binding.yaml", "v1.binding.yaml"));
        assert!(!match_binding_pattern("v?.binding.yaml", "v.binding.yaml"));
        assert!(!match_binding_pattern(
            "v?.binding.yaml",
            "v10.binding.yaml"
        ));
        // `?` matches a literal dot like any other character.
        assert!(match_binding_pattern(
            "a?b.binding.yaml",
            "a.b.binding.yaml"
        ));
    }

    #[test]
    fn test_match_binding_pattern_metachars_are_literal() {
        // Character classes and brace expansion are NOT supported.
        assert!(!match_binding_pattern(
            "[ab].binding.yaml",
            "a.binding.yaml"
        ));
        assert!(match_binding_pattern(
            "[ab].binding.yaml",
            "[ab].binding.yaml"
        ));
        assert!(!match_binding_pattern(
            "{a,b}.binding.yaml",
            "a.binding.yaml"
        ));
    }

    #[test]
    fn test_match_binding_pattern_is_case_sensitive_and_anchored() {
        assert!(!match_binding_pattern(
            "*.binding.yaml",
            "users.BINDING.yaml"
        ));
        assert!(!match_binding_pattern(
            "API-*.binding.yaml",
            "api-v1.binding.yaml"
        ));
        // A literal pattern is anchored at both ends, not a substring search.
        assert!(match_binding_pattern(
            "users.binding.yaml",
            "users.binding.yaml"
        ));
        assert!(!match_binding_pattern(
            "users.binding.yaml",
            "old-users.binding.yaml"
        ));
        // No leading star means anchored at the start.
        assert!(!match_binding_pattern(
            ".binding.yaml",
            "users.binding.yaml"
        ));
    }

    #[test]
    fn test_match_binding_pattern_iterates_unicode_code_points() {
        // One `?` consumes one character, not one UTF-8 byte...
        assert!(match_binding_pattern("?.binding.yaml", "é.binding.yaml"));
        assert!(!match_binding_pattern("??", "é"));
        // ...and not one UTF-16 code unit either: an astral character is a
        // single `char`, so exactly one `?` consumes it.
        assert!(match_binding_pattern(
            "?.binding.yaml",
            "\u{1F600}.binding.yaml"
        ));
        assert!(!match_binding_pattern(
            "??.binding.yaml",
            "\u{1F600}.binding.yaml"
        ));
    }

    #[test]
    fn test_match_binding_pattern_is_not_exponential() {
        // The naive recursive matcher would not return here.
        let name = "a".repeat(64);
        assert!(!match_binding_pattern("*a*a*a*a*b", &name));
    }

    // ---- A pattern is never rejected (A25 requirement 2) ----

    #[test]
    fn test_odd_patterns_yield_an_empty_selection_not_an_error() {
        // Every string is a valid pattern; the loader never errors on one for
        // a syntactic reason. These asserted the inverse until 0.13.0.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.binding.yaml"),
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": "x", "target": "pkg:f"}]}),
            )
            .unwrap(),
        )
        .unwrap();

        for odd in [
            "",
            "sub/*.binding.yaml",
            "**/*.binding.yaml",
            "*.binding.yaml/",
            "/",
            "a[b",
            "{x,y}",
        ] {
            let got = BindingLoader::new()
                .load_with_pattern(dir.path(), false, false, Some(odd))
                .unwrap_or_else(|e| panic!("pattern {odd:?} must not error, got {e:?}"));
            assert!(got.is_empty(), "pattern {odd:?} should select nothing");
        }
    }

    #[test]
    fn test_backslash_is_a_literal_not_a_path_separator() {
        // A25 requirement 4 names `\` a literal, so a file name carrying one
        // is matchable rather than the pattern being refused.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("sub\\x.binding.yaml"),
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": "x", "target": "pkg:f"}]}),
            )
            .unwrap(),
        )
        .unwrap();

        let got = BindingLoader::new()
            .load_with_pattern(dir.path(), false, false, Some("sub\\*.binding.yaml"))
            .unwrap();
        let ids: Vec<&str> = got.iter().map(|m| m.module_id.as_str()).collect();
        assert_eq!(ids, vec!["x"]);
    }

    #[test]
    fn test_missing_path_is_reported_as_such_not_as_a_pattern_error() {
        // An odd pattern used to pre-empt the real fault; there is no pattern
        // error any more, so `PathNotFound` is what surfaces.
        let missing = Path::new("/definitely/does/not/exist/anywhere");
        let err = BindingLoader::new()
            .load_with_pattern(missing, false, false, Some("**/*.binding.yaml"))
            .unwrap_err();
        assert!(
            matches!(err, BindingLoadError::PathNotFound { .. }),
            "expected PathNotFound, got {err:?}"
        );
    }

    #[test]
    fn test_load_with_pattern_selects_custom_pattern() {
        let dir = TempDir::new().unwrap();
        let doc = |id: &str| {
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": id, "target": "pkg:f"}]}),
            )
            .unwrap()
        };
        fs::write(dir.path().join("api-v1.cli.yaml"), doc("api.v1")).unwrap();
        fs::write(dir.path().join("web-v1.cli.yaml"), doc("web.v1")).unwrap();
        fs::write(dir.path().join("api-v1.binding.yaml"), doc("api.binding")).unwrap();

        let loader = BindingLoader::new();
        let matched = loader
            .load_with_pattern(dir.path(), false, false, Some("api-*.cli.yaml"))
            .unwrap();
        let ids: Vec<&str> = matched.iter().map(|m| m.module_id.as_str()).collect();
        assert_eq!(ids, vec!["api.v1"]);

        // `None` means the default pattern, and `load` delegates with `None`.
        let default_ids: Vec<String> = loader
            .load_with_pattern(dir.path(), false, false, None)
            .unwrap()
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        assert_eq!(default_ids, vec!["api.binding".to_string()]);
        let via_load: Vec<String> = loader
            .load(dir.path(), false, false)
            .unwrap()
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        assert_eq!(via_load, default_ids);
    }

    #[cfg(unix)]
    #[test]
    fn test_load_unreadable_real_file_still_fails_fast() {
        // The fail-soft carve-out is the file-type probe alone: a dangling
        // symlink is "not a file", not an error. A genuine read error on a
        // real, matching file keeps the existing fail-fast policy — this
        // pins that the softening did not widen.
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let unreadable = dir.path().join("locked.binding.yaml");
        fs::write(
            &unreadable,
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": "locked.mod", "target": "pkg:f"}]}),
            )
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

        // Mode 0o000 does not stop root, and some CI images run as root.
        // Probe directly rather than guessing, so this never fails spuriously.
        let permissions_are_enforced = fs::read_to_string(&unreadable).is_err();
        let result = BindingLoader::new().load(dir.path(), false, false);

        // Restore before asserting so the TempDir can always clean up.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();

        if !permissions_are_enforced {
            eprintln!("SKIP: file permissions are not enforced for this user");
            return;
        }
        assert!(
            matches!(result, Err(BindingLoadError::FileRead { .. })),
            "expected FileRead on an unreadable real file, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_load_with_pattern_skips_dangling_symlink_in_both_branches() {
        // A dangling symlink whose NAME matches is skipped, not read. The
        // following stat that makes a symlinked file selectable returns false
        // here instead of erroring, and that failure must be swallowed as
        // "not a file" rather than escaping as `FileRead` and aborting the
        // whole load. The recursive branch is the one that bites: with
        // `follow_links(false)` walkdir yields the dangling link as an entry,
        // and a guard phrased as "not a directory" would let it through.
        let dir = TempDir::new().unwrap();
        let doc = |id: &str| {
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": id, "target": "pkg:f"}]}),
            )
            .unwrap()
        };

        fs::write(dir.path().join("a.binding.yaml"), doc("a.mod")).unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("b.binding.yaml"), doc("b.mod")).unwrap();

        // Targets intentionally never created — at the root and nested.
        std::os::unix::fs::symlink(
            dir.path().join("does-not-exist.binding.yaml"),
            dir.path().join("dangling.binding.yaml"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            nested.join("does-not-exist.binding.yaml"),
            nested.join("dangling.binding.yaml"),
        )
        .unwrap();

        let loader = BindingLoader::new();

        let flat: Vec<String> = loader
            .load(dir.path(), false, false)
            .expect("a dangling symlink must be skipped, not read")
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        assert_eq!(flat, vec!["a.mod".to_string()]);

        let recursive: Vec<String> = loader
            .load(dir.path(), false, true)
            .expect("a dangling symlink must be skipped, not read")
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        assert_eq!(
            recursive,
            vec!["a.mod".to_string(), "b.mod".to_string()],
            "sorted by path: a.binding.yaml before nested/b.binding.yaml"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_load_with_pattern_follows_file_symlinks_but_not_directory_symlinks() {
        // Test the target, not the link: a symlink to a regular file IS
        // selected, while a symlink to a directory is neither selected nor
        // descended into, and a broken symlink is skipped like any non-file.
        let dir = TempDir::new().unwrap();
        let doc = |id: &str| {
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": id, "target": "pkg:f"}]}),
            )
            .unwrap()
        };

        let real = dir.path().join("real.binding.yaml");
        fs::write(&real, doc("real.mod")).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("alias.binding.yaml")).unwrap();

        let target_dir = dir.path().join("target");
        fs::create_dir(&target_dir).unwrap();
        fs::write(target_dir.join("inner.binding.yaml"), doc("inner.mod")).unwrap();
        std::os::unix::fs::symlink(&target_dir, dir.path().join("link.binding.yaml")).unwrap();

        std::os::unix::fs::symlink(
            dir.path().join("nowhere"),
            dir.path().join("broken.binding.yaml"),
        )
        .unwrap();

        let loader = BindingLoader::new();

        // Flat: the file symlink is followed; the directory symlink and the
        // broken symlink are not candidates.
        let mut flat: Vec<String> = loader
            .load(dir.path(), false, false)
            .expect("symlinks must not error")
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        flat.sort();
        assert_eq!(flat, vec!["real.mod".to_string(), "real.mod".to_string()]);

        // Recursive: the real directory is traversed normally, so the inner
        // file appears exactly once — the symlinked directory is not walked.
        let mut rec: Vec<String> = loader
            .load(dir.path(), false, true)
            .expect("symlinks must not error")
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        rec.sort();
        assert_eq!(
            rec,
            vec![
                "inner.mod".to_string(),
                "real.mod".to_string(),
                "real.mod".to_string(),
            ]
        );
    }

    #[test]
    fn test_load_with_pattern_validates_even_when_path_is_a_file() {
        // "Ignored for single files" governs MATCHING only, never validation:
        // pattern validation is a pure precondition on the argument and runs
        // before the file/directory probe.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("one.binding.yaml");
        fs::write(
            &file,
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": "one.mod", "target": "pkg:f"}]}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(file.is_file());

        // 0.12.0 validated the pattern even here, so these raised. 0.13.0
        // removed validation, so "ignored for a single file" now means
        // ignored outright: the named file loads whatever the pattern says.
        for odd in ["", "**/*.yaml"] {
            let got = BindingLoader::new()
                .load_with_pattern(&file, false, false, Some(odd))
                .unwrap_or_else(|e| panic!("pattern {odd:?} must not error on a file, got {e:?}"));
            let ids: Vec<&str> = got.iter().map(|m| m.module_id.as_str()).collect();
            assert_eq!(ids, vec!["one.mod"], "pattern {odd:?} must be ignored");
        }
    }

    #[test]
    fn test_load_with_pattern_skips_directory_named_like_a_match() {
        // A directory whose name matches the pattern is not a candidate at any
        // depth: no module, and no read-time error.
        let dir = TempDir::new().unwrap();
        let decoy = dir.path().join("decoy.binding.yaml");
        fs::create_dir(&decoy).unwrap();
        fs::write(decoy.join("inner.binding.yaml"), "not: [valid").unwrap();
        fs::write(
            dir.path().join("real.binding.yaml"),
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": "real.mod", "target": "pkg:f"}]}),
            )
            .unwrap(),
        )
        .unwrap();

        let flat = BindingLoader::new()
            .load_with_pattern(dir.path(), false, false, Some("*.binding.yaml"))
            .expect("a directory named like a match must be skipped, not read");
        let ids: Vec<&str> = flat.iter().map(|m| m.module_id.as_str()).collect();
        assert_eq!(ids, vec!["real.mod"]);
    }

    #[test]
    fn test_load_with_pattern_ignored_for_single_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("odd-name.yaml");
        fs::write(
            &file,
            serde_yaml_ng::to_string(
                &json!({"spec_version": "1.0", "bindings": [{"module_id": "odd.one", "target": "pkg:f"}]}),
            )
            .unwrap(),
        )
        .unwrap();

        let modules = BindingLoader::new()
            .load_with_pattern(&file, false, false, Some("*.binding.yaml"))
            .unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].module_id, "odd.one");
    }
}
