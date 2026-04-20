// DisplayResolver — sparse binding.yaml display overlay (§5.13).
//
// Resolves surface-facing presentation fields (alias, description, guidance)
// for each ScannedModule by merging:
//   surface-specific override > display default > binding-level > scanner value
//
// The resolved fields are stored in ScannedModule.metadata["display"] and
// travel through RegistryWriter into FunctionModule.metadata["display"],
// where CLI/MCP/A2A surfaces read them at render time.

use std::collections::HashMap;
use std::path::Path;

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

static MCP_ALIAS_SANITIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9_-]").expect("valid regex"));
static MCP_ALIAS_PATTERN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_-]*$").expect("valid regex"));
static CLI_ALIAS_PATTERN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]*$").expect("valid regex"));

use crate::types::ScannedModule;

const MCP_ALIAS_MAX: usize = 64;

/// Resolves display overlay fields for a list of ScannedModules.
///
/// # Usage
///
/// ```ignore
/// let resolver = DisplayResolver::new();
/// let resolved = resolver.resolve(modules, None, None);
/// ```
///
/// The returned list contains the same ScannedModules with
/// `metadata["display"]` populated for all surfaces.
#[derive(Debug, Default)]
pub struct DisplayResolver;

impl DisplayResolver {
    /// Create a new DisplayResolver.
    pub fn new() -> Self {
        Self
    }

    /// Apply display overlay to a list of ScannedModules.
    ///
    /// # Arguments
    ///
    /// * `modules` - ScannedModule instances from a framework scanner.
    /// * `binding_path` - Path to a single `.binding.yaml` file or a directory
    ///   of binding files. Optional.
    /// * `binding_data` - Pre-parsed binding YAML content as a JSON Value
    ///   (`{"bindings": [...]}`) or a `module_id -> entry` map.
    ///   Takes precedence over `binding_path`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if an MCP alias exceeds the 64-character limit or does not
    /// match the required pattern.
    pub fn resolve(
        &self,
        modules: Vec<ScannedModule>,
        binding_path: Option<&Path>,
        binding_data: Option<&Value>,
    ) -> Result<Vec<ScannedModule>, DisplayResolverError> {
        let binding_map = self.build_binding_map(binding_path, binding_data);

        if !binding_map.is_empty() {
            let matched = modules
                .iter()
                .filter(|m| binding_map.contains_key(&m.module_id))
                .count();
            info!(
                "DisplayResolver: {}/{} modules matched binding entries.",
                matched,
                modules.len(),
            );
            if matched == 0 {
                warn!(
                    "DisplayResolver: binding map loaded {} entries but none matched \
                     any scanned module_id — check binding.yaml module_id values.",
                    binding_map.len(),
                );
            }
        }

        modules
            .into_iter()
            .map(|m| self.resolve_one(m, &binding_map))
            .collect()
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Build a module_id -> binding-entry map from the provided sources.
    fn build_binding_map(
        &self,
        binding_path: Option<&Path>,
        binding_data: Option<&Value>,
    ) -> HashMap<String, Value> {
        if let Some(data) = binding_data {
            return Self::parse_binding_data(data);
        }
        if let Some(path) = binding_path {
            return self.load_binding_files(path);
        }
        HashMap::new()
    }

    /// Parse pre-loaded binding data.
    ///
    /// Accepts either `{"bindings": [...]}` or a direct `module_id -> entry` map.
    fn parse_binding_data(data: &Value) -> HashMap<String, Value> {
        let mut result = HashMap::new();

        // Accept {"bindings": [...]} format
        if let Some(bindings) = data.get("bindings").and_then(|v| v.as_array()) {
            for entry in bindings {
                if let Some(module_id) = entry.get("module_id").and_then(|v| v.as_str()) {
                    result.insert(module_id.to_string(), entry.clone());
                }
            }
            return result;
        }

        // Already a map: module_id -> entry
        if let Some(obj) = data.as_object() {
            for (k, v) in obj {
                if v.is_object() {
                    result.insert(k.clone(), v.clone());
                }
            }
        }

        result
    }

    /// Load binding files from a path (file or directory).
    ///
    /// Per-entry I/O failures (permission denied, unreadable symlinks) are
    /// surfaced via `tracing::warn!` rather than silently dropped. This
    /// mirrors the error-handling contract of
    /// [`crate::binding_loader::BindingLoader::load`] — the two loaders
    /// traverse the same directory shape and should behave consistently
    /// when the filesystem misbehaves. The return type stays
    /// `HashMap<String, Value>` (rather than `Result<…, …>`) to preserve
    /// backward compatibility for 0.5.x.
    fn load_binding_files(&self, path: &Path) -> HashMap<String, Value> {
        let mut result = HashMap::new();

        let files: Vec<std::path::PathBuf> = if path.is_file() {
            vec![path.to_path_buf()]
        } else if path.is_dir() {
            let mut entries: Vec<std::path::PathBuf> = Vec::new();
            match std::fs::read_dir(path) {
                Ok(read_dir) => {
                    for entry_result in read_dir {
                        match entry_result {
                            Ok(entry) => {
                                let p = entry.path();
                                let is_binding = p
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .is_some_and(|n| n.ends_with(".binding.yaml"));
                                if is_binding {
                                    entries.push(p);
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "DisplayResolver: skipping unreadable entry in {:?}: {}",
                                    path, e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "DisplayResolver: failed to read binding directory {:?}: {}",
                        path, e
                    );
                    return result;
                }
            }
            entries.sort();
            entries
        } else {
            warn!("DisplayResolver: binding path not found: {:?}", path);
            return result;
        };

        for f in files {
            match std::fs::read_to_string(&f) {
                Ok(content) => match serde_yaml_ng::from_str::<Value>(&content) {
                    Ok(data) => {
                        let parsed = Self::parse_binding_data(&data);
                        result.extend(parsed);
                    }
                    Err(e) => {
                        warn!("DisplayResolver: failed to parse {:?}: {}", f, e);
                    }
                },
                Err(e) => {
                    warn!("DisplayResolver: failed to load {:?}: {}", f, e);
                }
            }
        }

        result
    }

    /// Resolve display fields for a single ScannedModule.
    fn resolve_one(
        &self,
        mut module: ScannedModule,
        binding_map: &HashMap<String, Value>,
    ) -> Result<ScannedModule, DisplayResolverError> {
        let empty_obj = json!({});
        let entry = binding_map.get(&module.module_id).unwrap_or(&empty_obj);
        let display_cfg = entry.get("display").unwrap_or(&empty_obj);

        let defaults = compute_display_defaults(&module, entry, display_cfg);

        let (cli_surface, cli_alias_explicit) = self.resolve_surface(
            display_cfg,
            "cli",
            &defaults.alias,
            &defaults.description,
            &defaults.guidance,
        );
        let (mut mcp_surface, _) = self.resolve_surface(
            display_cfg,
            "mcp",
            &defaults.alias,
            &defaults.description,
            &defaults.guidance,
        );
        let (a2a_surface, _) = self.resolve_surface(
            display_cfg,
            "a2a",
            &defaults.alias,
            &defaults.description,
            &defaults.guidance,
        );

        let raw_mcp_alias = mcp_surface
            .get("alias")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sanitized = sanitize_mcp_alias(&raw_mcp_alias);
        if sanitized != raw_mcp_alias {
            debug!(
                "Module '{}': MCP alias auto-sanitized '{}' → '{}'.",
                module.module_id, raw_mcp_alias, sanitized
            );
        }
        mcp_surface["alias"] = json!(sanitized);

        let mut display = assemble_display(&defaults, cli_surface, mcp_surface, a2a_surface);
        self.validate_aliases(&mut display, &module.module_id, cli_alias_explicit)?;

        module.metadata.insert("display".into(), display);
        Ok(module)
    }

    /// Resolve fields for a single surface (cli, mcp, or a2a).
    ///
    /// Returns `(surface_dict, alias_was_explicit)`.
    fn resolve_surface(
        &self,
        display_cfg: &Value,
        key: &str,
        default_alias: &str,
        default_description: &str,
        default_guidance: &Option<String>,
    ) -> (Value, bool) {
        let empty = json!({});
        let sc = display_cfg.get(key).unwrap_or(&empty);
        let alias_explicit = sc.get("alias").and_then(|v| v.as_str()).is_some();

        let alias = str_or(sc, "alias").unwrap_or(default_alias);
        let description = str_or(sc, "description").unwrap_or(default_description);
        let guidance = str_or(sc, "guidance")
            .map(|s| s.to_string())
            .or_else(|| default_guidance.clone());

        let mut surface = json!({
            "alias": alias,
            "description": description,
        });
        if let Some(g) = &guidance {
            surface["guidance"] = json!(g);
        } else {
            surface["guidance"] = Value::Null;
        }

        (surface, alias_explicit)
    }

    /// Validate surface alias constraints per §5.13.6.
    fn validate_aliases(
        &self,
        display: &mut Value,
        module_id: &str,
        cli_alias_explicit: bool,
    ) -> Result<(), DisplayResolverError> {
        let mcp_alias_pattern = &*MCP_ALIAS_PATTERN_RE;
        let cli_alias_pattern = &*CLI_ALIAS_PATTERN_RE;

        // MCP: enforce 64-char hard limit (alias was already auto-sanitized)
        let mcp_alias = display["mcp"]["alias"].as_str().unwrap_or("").to_string();

        if mcp_alias.len() > MCP_ALIAS_MAX {
            return Err(DisplayResolverError::Validation(format!(
                "Module '{}': MCP alias '{}' exceeds {}-character hard limit (OpenAI spec). \
                 Set display.mcp.alias to a shorter value.",
                module_id, mcp_alias, MCP_ALIAS_MAX,
            )));
        }
        if !mcp_alias_pattern.is_match(&mcp_alias) {
            return Err(DisplayResolverError::Validation(format!(
                "Module '{}': MCP alias '{}' does not match \
                 required pattern ^[a-zA-Z_][a-zA-Z0-9_-]*$.",
                module_id, mcp_alias,
            )));
        }

        // CLI: only validate user-explicitly-set aliases
        if cli_alias_explicit {
            let cli_alias = display["cli"]["alias"].as_str().unwrap_or("").to_string();
            if !cli_alias_pattern.is_match(&cli_alias) {
                let default_alias = display["alias"].as_str().unwrap_or("").to_string();
                warn!(
                    "Module '{}': CLI alias '{}' does not match shell-safe pattern \
                     ^[a-z][a-z0-9_-]*$ — falling back to default alias '{}'.",
                    module_id, cli_alias, default_alias,
                );
                display["cli"]["alias"] = json!(default_alias);
            }
        }

        Ok(())
    }
}

/// Errors returned by [`DisplayResolver`] operations.
#[derive(Debug, thiserror::Error)]
pub enum DisplayResolverError {
    /// An alias validation constraint was violated.
    #[error("{0}")]
    Validation(String),
}

// -- Module-level helpers extracted from resolve_one --

/// Resolved cross-surface display defaults before per-surface overrides.
struct DisplayDefaults {
    alias: String,
    description: String,
    documentation: Option<String>,
    guidance: Option<String>,
    tags: Vec<String>,
}

/// Compute cross-surface display defaults from binding and scanner data.
fn compute_display_defaults(
    module: &ScannedModule,
    entry: &Value,
    display_cfg: &Value,
) -> DisplayDefaults {
    let binding_desc = entry.get("description").and_then(|v| v.as_str());
    let binding_docs = entry.get("documentation").and_then(|v| v.as_str());

    // Top-level suggested_alias takes precedence over metadata["suggested_alias"].
    let field_alias = module
        .suggested_alias
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let metadata_alias = module
        .metadata
        .get("suggested_alias")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let suggested_alias = field_alias.or(metadata_alias);

    let alias = str_or(display_cfg, "alias")
        .or(suggested_alias.as_deref())
        .unwrap_or(&module.module_id)
        .to_string();
    let description = str_or(display_cfg, "description")
        .or(binding_desc)
        .unwrap_or(&module.description)
        .to_string();
    let documentation = str_or(display_cfg, "documentation")
        .or(binding_docs)
        .or(module.documentation.as_deref())
        .map(|s| s.to_string());
    let guidance = str_or(display_cfg, "guidance").map(|s| s.to_string());
    let tags = tags_or(display_cfg, "tags")
        .or_else(|| tags_or(entry, "tags"))
        .unwrap_or_else(|| module.tags.clone());

    DisplayDefaults {
        alias,
        description,
        documentation,
        guidance,
        tags,
    }
}

/// Sanitize a raw MCP alias: replace disallowed characters with `_` and
/// prefix a leading digit with `_` (OpenAI function-name rules).
fn sanitize_mcp_alias(raw: &str) -> String {
    let mut s = MCP_ALIAS_SANITIZE_RE.replace_all(raw, "_").to_string();
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s = format!("_{s}");
    }
    s
}

/// Assemble the final display JSON from resolved defaults and surface values.
fn assemble_display(defaults: &DisplayDefaults, cli: Value, mcp: Value, a2a: Value) -> Value {
    let mut display = json!({
        "alias": defaults.alias,
        "description": defaults.description,
        "guidance": defaults.guidance,
        "tags": defaults.tags,
        "cli": cli,
        "mcp": mcp,
        "a2a": a2a,
    });
    display["documentation"] = match &defaults.documentation {
        Some(doc) => json!(doc),
        None => Value::Null,
    };
    display
}

// -- Utility helpers --

/// Extract a non-empty string field from a JSON value.
fn str_or<'a>(val: &'a Value, key: &str) -> Option<&'a str> {
    val.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Extract a tags array from a JSON value.
fn tags_or(val: &Value, key: &str) -> Option<Vec<String>> {
    val.get(key).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper to create a minimal ScannedModule for testing.
    fn make_module(module_id: &str, description: &str) -> ScannedModule {
        ScannedModule::new(
            module_id.into(),
            description.into(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec!["default-tag".into()],
            format!("app:{module_id}"),
        )
    }

    #[test]
    fn test_new_creates_default_instance() {
        let resolver = DisplayResolver::new();
        // Just verify it can be created — it has no configuration.
        let _ = format!("{:?}", resolver);
    }

    #[test]
    fn test_resolve_passthrough_no_bindings() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("users.get", "Get a user")];
        let resolved = resolver.resolve(modules, None, None).unwrap();

        assert_eq!(resolved.len(), 1);
        let display = resolved[0].metadata.get("display").unwrap();
        // Default alias falls back to module_id
        assert_eq!(display["alias"], "users.get");
        assert_eq!(display["description"], "Get a user");
    }

    #[test]
    fn test_resolve_with_binding_data_map_format() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("users.get", "Get a user")];

        let binding_data = json!({
            "users.get": {
                "display": {
                    "alias": "get-user",
                    "description": "Retrieve a user by ID",
                    "guidance": "Use when you know the user ID"
                }
            }
        });

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["alias"], "get-user");
        assert_eq!(display["description"], "Retrieve a user by ID");
        assert_eq!(display["guidance"], "Use when you know the user ID");
    }

    #[test]
    fn test_resolve_with_binding_data_bindings_list_format() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("users.get", "Get a user")];

        let binding_data = json!({
            "bindings": [
                {
                    "module_id": "users.get",
                    "description": "Binding-level desc",
                    "display": {
                        "alias": "get-user"
                    }
                }
            ]
        });

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["alias"], "get-user");
        // description comes from display first, then binding-level, then scanner
        assert_eq!(display["description"], "Binding-level desc");
    }

    #[test]
    fn test_resolution_chain_precedence() {
        // surface-specific > display default > binding-level > scanner value
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("users.get", "Scanner desc")];

        let binding_data = json!({
            "users.get": {
                "description": "Binding desc",
                "display": {
                    "description": "Display desc",
                    "cli": {
                        "description": "CLI desc"
                    }
                }
            }
        });

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // Top-level uses display default
        assert_eq!(display["description"], "Display desc");
        // CLI surface uses its own override
        assert_eq!(display["cli"]["description"], "CLI desc");
        // MCP falls through to display default
        assert_eq!(display["mcp"]["description"], "Display desc");
    }

    #[test]
    fn test_mcp_alias_auto_sanitization_dots() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("image.resize", "Resize image")];

        let resolved = resolver.resolve(modules, None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // Dots get replaced with underscores
        assert_eq!(display["mcp"]["alias"], "image_resize");
    }

    #[test]
    fn test_mcp_alias_auto_sanitization_spaces() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("users.get user", "Get user")];

        let resolved = resolver.resolve(modules, None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["mcp"]["alias"], "users_get_user");
    }

    #[test]
    fn test_mcp_alias_leading_digit_prefix() {
        let resolver = DisplayResolver::new();
        let binding_data = json!({
            "test": {
                "display": {
                    "alias": "1get-user"
                }
            }
        });
        let modules = vec![make_module("test", "Test")];

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["mcp"]["alias"], "_1get-user");
    }

    #[test]
    fn test_mcp_alias_exceeds_max_length() {
        let resolver = DisplayResolver::new();
        let long_alias = "a".repeat(65);
        let binding_data = json!({
            "test": {
                "display": {
                    "alias": long_alias
                }
            }
        });
        let modules = vec![make_module("test", "Test")];

        let result = resolver.resolve(modules, None, Some(&binding_data));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("exceeds 64-character hard limit"));
    }

    #[test]
    fn test_mcp_alias_invalid_pattern() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("test", "Test")];

        // Test with a binding that would produce an invalid MCP alias.
        let binding_data2 = json!({
            "test": {
                "display": {
                    "mcp": {
                        "alias": "---invalid"
                    }
                }
            }
        });
        let result = resolver.resolve(modules, None, Some(&binding_data2));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn test_cli_alias_explicit_invalid_falls_back() {
        let resolver = DisplayResolver::new();
        let binding_data = json!({
            "users.get": {
                "display": {
                    "alias": "get-user",
                    "cli": {
                        "alias": "Get-User"
                    }
                }
            }
        });
        let modules = vec![make_module("users.get", "Get user")];

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // CLI alias should fall back to default alias because "Get-User" is invalid
        assert_eq!(display["cli"]["alias"], "get-user");
    }

    #[test]
    fn test_cli_alias_non_explicit_not_validated() {
        // When CLI alias comes from scanner (not explicitly set), no validation
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("MyModule", "Description")];

        // No binding data, so CLI alias inherits from module_id which has uppercase
        let resolved = resolver.resolve(modules, None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // Should NOT fall back — accepts non-conforming alias from scanner
        assert_eq!(display["cli"]["alias"], "MyModule");
    }

    #[test]
    fn test_suggested_alias_fallback() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("users__get_user", "Get user");
        module
            .metadata
            .insert("suggested_alias".into(), json!("get_user"));

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // suggested_alias should be used instead of module_id
        assert_eq!(display["alias"], "get_user");
    }

    // ---- Dual-source suggested_alias resolution ----

    #[test]
    fn test_suggested_alias_field_only() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("tasks.user_data.post", "Create");
        module.suggested_alias = Some("tasks.user_data.create".into());

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "tasks.user_data.create");
    }

    #[test]
    fn test_suggested_alias_metadata_only() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("tasks.user_data.post", "Create");
        module
            .metadata
            .insert("suggested_alias".into(), json!("tasks.user_data.legacy"));

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "tasks.user_data.legacy");
    }

    #[test]
    fn test_suggested_alias_field_precedence_over_metadata() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("tasks.user_data.post", "Create");
        module.suggested_alias = Some("tasks.user_data.create".into());
        module
            .metadata
            .insert("suggested_alias".into(), json!("tasks.user_data.legacy"));

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "tasks.user_data.create");
    }

    #[test]
    fn test_suggested_alias_empty_field_falls_through_to_metadata() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("tasks.user_data.post", "Create");
        module.suggested_alias = Some("".into());
        module
            .metadata
            .insert("suggested_alias".into(), json!("tasks.user_data.legacy"));

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "tasks.user_data.legacy");
    }

    #[test]
    fn test_suggested_alias_none_field_falls_through_to_metadata() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("tasks.user_data.post", "Create");
        module.suggested_alias = None;
        module
            .metadata
            .insert("suggested_alias".into(), json!("tasks.user_data.legacy"));

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "tasks.user_data.legacy");
    }

    #[test]
    fn test_suggested_alias_neither_falls_through_to_module_id() {
        let resolver = DisplayResolver::new();
        let module = make_module("tasks.user_data.post", "Create");
        // Neither field nor metadata alias set.

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "tasks.user_data.post");
    }

    #[test]
    fn test_tags_resolution_from_display() {
        let resolver = DisplayResolver::new();
        let binding_data = json!({
            "test": {
                "tags": ["binding-tag"],
                "display": {
                    "tags": ["display-tag"]
                }
            }
        });
        let modules = vec![make_module("test", "Test")];

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // display.tags takes precedence over entry.tags
        let tags: Vec<String> = display["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tags, vec!["display-tag"]);
    }

    #[test]
    fn test_tags_resolution_from_binding_entry() {
        let resolver = DisplayResolver::new();
        let binding_data = json!({
            "test": {
                "tags": ["binding-tag"]
            }
        });
        let modules = vec![make_module("test", "Test")];

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        let tags: Vec<String> = display["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tags, vec!["binding-tag"]);
    }

    #[test]
    fn test_tags_fallback_to_scanner() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("test", "Test")];

        let resolved = resolver.resolve(modules, None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        let tags: Vec<String> = display["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tags, vec!["default-tag"]);
    }

    #[test]
    fn test_documentation_resolution() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("test", "Test");
        module.documentation = Some("Scanner docs".into());

        let binding_data = json!({
            "test": {
                "documentation": "Binding docs",
                "display": {
                    "documentation": "Display docs"
                }
            }
        });

        let resolved = resolver
            .resolve(vec![module], None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["documentation"], "Display docs");
    }

    #[test]
    fn test_documentation_fallback_to_binding() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("test", "Test");
        module.documentation = Some("Scanner docs".into());

        let binding_data = json!({
            "test": {
                "documentation": "Binding docs"
            }
        });

        let resolved = resolver
            .resolve(vec![module], None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["documentation"], "Binding docs");
    }

    #[test]
    fn test_documentation_fallback_to_scanner() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("test", "Test");
        module.documentation = Some("Scanner docs".into());

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["documentation"], "Scanner docs");
    }

    #[test]
    fn test_multiple_modules() {
        let resolver = DisplayResolver::new();
        let modules = vec![
            make_module("mod_a", "Module A"),
            make_module("mod_b", "Module B"),
            make_module("mod_c", "Module C"),
        ];

        let binding_data = json!({
            "mod_a": {
                "display": { "alias": "alias-a" }
            },
            "mod_c": {
                "display": { "alias": "alias-c" }
            }
        });

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].metadata["display"]["alias"], "alias-a");
        assert_eq!(resolved[1].metadata["display"]["alias"], "mod_b");
        assert_eq!(resolved[2].metadata["display"]["alias"], "alias-c");
    }

    #[test]
    fn test_binding_map_zero_matches_still_resolves() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("actual_id", "Description")];

        let binding_data = json!({
            "nonexistent_id": {
                "display": { "alias": "nope" }
            }
        });

        // Should still succeed, just no bindings applied
        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].metadata["display"]["alias"], "actual_id");
    }

    #[test]
    fn test_parse_binding_data_bindings_list() {
        let data = json!({
            "bindings": [
                { "module_id": "a", "description": "Module A" },
                { "module_id": "b", "description": "Module B" },
                { "description": "No ID — should be skipped" }
            ]
        });
        let map = DisplayResolver::parse_binding_data(&data);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("a"));
        assert!(map.contains_key("b"));
    }

    #[test]
    fn test_parse_binding_data_map() {
        let data = json!({
            "a": { "display": { "alias": "alias-a" } },
            "b": { "display": { "alias": "alias-b" } },
            "scalar": "not-an-object"
        });
        let map = DisplayResolver::parse_binding_data(&data);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("a"));
        assert!(map.contains_key("b"));
    }

    #[test]
    fn test_load_binding_files_single_file() {
        let resolver = DisplayResolver::new();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.binding.yaml");
        std::fs::write(
            &file_path,
            "bindings:\n  - module_id: test\n    description: From file\n",
        )
        .unwrap();

        let map = resolver.load_binding_files(&file_path);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("test"));
    }

    #[test]
    fn test_load_binding_files_directory() {
        let resolver = DisplayResolver::new();
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("a.binding.yaml"),
            "bindings:\n  - module_id: a\n    description: A\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.binding.yaml"),
            "bindings:\n  - module_id: b\n    description: B\n",
        )
        .unwrap();
        // Non-matching file should be ignored
        std::fs::write(dir.path().join("c.yaml"), "bindings:\n  - module_id: c\n").unwrap();

        let map = resolver.load_binding_files(dir.path());
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("a"));
        assert!(map.contains_key("b"));
        assert!(!map.contains_key("c"));
    }

    #[test]
    fn test_load_binding_files_nonexistent_path() {
        let resolver = DisplayResolver::new();
        let map = resolver.load_binding_files(Path::new("/nonexistent/path"));
        assert!(map.is_empty());
    }

    #[test]
    fn test_load_binding_files_invalid_yaml() {
        let resolver = DisplayResolver::new();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("bad.binding.yaml");
        std::fs::write(&file_path, "{{{{not valid yaml").unwrap();

        let map = resolver.load_binding_files(&file_path);
        assert!(map.is_empty());
    }

    #[test]
    fn test_surface_fields_populated() {
        let resolver = DisplayResolver::new();
        let modules = vec![make_module("test_mod", "Test desc")];

        let resolved = resolver.resolve(modules, None, None).unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // All three surfaces should be present
        assert!(display.get("cli").is_some());
        assert!(display.get("mcp").is_some());
        assert!(display.get("a2a").is_some());

        // Each surface should have alias and description
        for surface in &["cli", "mcp", "a2a"] {
            assert!(display[surface].get("alias").is_some());
            assert!(display[surface].get("description").is_some());
        }
    }

    #[test]
    fn test_mcp_alias_valid_stays_unchanged() {
        let resolver = DisplayResolver::new();
        let binding_data = json!({
            "test": {
                "display": {
                    "mcp": {
                        "alias": "valid_alias-123"
                    }
                }
            }
        });
        let modules = vec![make_module("test", "Test")];

        let resolved = resolver
            .resolve(modules, None, Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        assert_eq!(display["mcp"]["alias"], "valid_alias-123");
    }

    #[test]
    fn test_binding_data_takes_precedence_over_path() {
        let resolver = DisplayResolver::new();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.binding.yaml");
        std::fs::write(
            &file_path,
            "bindings:\n  - module_id: test\n    display:\n      alias: from-file\n",
        )
        .unwrap();

        let binding_data = json!({
            "test": {
                "display": { "alias": "from-data" }
            }
        });

        let modules = vec![make_module("test", "Test")];
        let resolved = resolver
            .resolve(modules, Some(file_path.as_path()), Some(&binding_data))
            .unwrap();
        let display = resolved[0].metadata.get("display").unwrap();

        // binding_data should win over binding_path
        assert_eq!(display["alias"], "from-data");
    }

    #[test]
    fn test_mcp_alias_64_chars_exactly_ok() {
        let resolver = DisplayResolver::new();
        let alias_64 = "a".repeat(64);
        let binding_data = json!({
            "test": {
                "display": {
                    "alias": alias_64
                }
            }
        });
        let modules = vec![make_module("test", "Test")];

        let result = resolver.resolve(modules, None, Some(&binding_data));
        assert!(result.is_ok());
    }

    #[test]
    fn test_empty_modules_list() {
        let resolver = DisplayResolver::new();
        let resolved = resolver.resolve(vec![], None, None).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn test_original_metadata_preserved() {
        let resolver = DisplayResolver::new();
        let mut module = make_module("test", "Test");
        module
            .metadata
            .insert("custom_key".into(), json!("custom_value"));

        let resolved = resolver.resolve(vec![module], None, None).unwrap();
        assert_eq!(resolved[0].metadata["custom_key"], "custom_value");
        assert!(resolved[0].metadata.contains_key("display"));
    }

    /// Behavioural guard for D2-1: `load_binding_files` must keep loading
    /// the readable entries when the directory contains non-binding content
    /// (e.g. a subdirectory or an unrelated file). Previously the whole
    /// traversal used `filter_map(Result::ok)` and silently dropped errors;
    /// the new iteration walks per-entry with structured `warn!` on I/O
    /// failures. This test exercises the happy-path continuation — a true
    /// per-entry I/O error is hard to provoke deterministically in CI.
    #[test]
    fn test_load_binding_files_ignores_non_binding_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ok.binding.yaml"),
            "bindings:\n  - module_id: ok_mod\n    display:\n      alias: ok\n",
        )
        .unwrap();
        // Sibling subdirectory and unrelated file that must not abort the load.
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a binding file").unwrap();

        let resolver = DisplayResolver::new();
        let resolved = resolver
            .resolve(vec![make_module("ok_mod", "ok")], Some(dir.path()), None)
            .unwrap();

        assert_eq!(resolved.len(), 1);
        let display = resolved[0].metadata.get("display").unwrap();
        assert_eq!(display["alias"], "ok");
    }
}
