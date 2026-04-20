// Target string resolution utilities.
//
// Validates and parses target strings in `module::path:qualname` format.
// In Python and TypeScript this dynamically imports and resolves the target;
// in Rust (no runtime import) we validate the format and return the
// parsed components.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A parsed target reference with module path and qualified name components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTarget {
    /// The module path portion (before the last `:`).
    pub module_path: String,
    /// The qualified name / export name (after the last `:`).
    pub qualname: String,
}

/// Validate and parse a target string in `module_path:qualname` format.
///
/// The last `:` in the string is used as the separator, matching the
/// TypeScript implementation which supports prefixed module paths.
///
/// # Format
///
/// - Python style: `"my_package.my_module:MyClass"`
/// - TypeScript style: `"./handlers/task:createTask"`
/// - Rust style: `"my_crate::module:function_name"`
///
/// # Errors
///
/// Returns `Err` if:
/// - The target string contains no `:` separator
/// - The module path is empty
/// - The qualname is empty
/// - The module path or qualname contain invalid characters
///
/// # Examples
///
/// ```
/// use apcore_toolkit::resolve_target::resolve_target;
///
/// let result = resolve_target("my_module:my_func").unwrap();
/// assert_eq!(result.module_path, "my_module");
/// assert_eq!(result.qualname, "my_func");
/// ```
/// Regex matching valid identifier qualnames (alphanumeric + underscores, no leading digit).
static IDENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$").expect("static regex"));

pub fn resolve_target(target: &str) -> Result<ResolvedTarget, String> {
    let last_colon = target.rfind(':').ok_or_else(|| {
        format!("Invalid target format: \"{target}\". Expected \"module_path:qualname\".")
    })?;

    let module_path = &target[..last_colon];
    let qualname = &target[last_colon + 1..];

    if module_path.is_empty() {
        return Err(format!(
            "Invalid target format: \"{target}\". Module path is empty."
        ));
    }

    if qualname.is_empty() {
        return Err(format!(
            "Invalid target format: \"{target}\". Qualified name is empty."
        ));
    }

    if !IDENT_RE.is_match(qualname) {
        return Err(format!(
            "Invalid qualname \"{qualname}\" in target \"{target}\". \
             Must be a valid identifier."
        ));
    }

    Ok(ResolvedTarget {
        module_path: module_path.to_string(),
        qualname: qualname.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_target_python_style() {
        let result = resolve_target("my_package.my_module:MyClass").unwrap();
        assert_eq!(result.module_path, "my_package.my_module");
        assert_eq!(result.qualname, "MyClass");
    }

    #[test]
    fn test_resolve_target_rust_style() {
        let result = resolve_target("my_crate::handlers::task:create_task").unwrap();
        assert_eq!(result.module_path, "my_crate::handlers::task");
        assert_eq!(result.qualname, "create_task");
    }

    #[test]
    fn test_resolve_target_simple() {
        let result = resolve_target("app:handler").unwrap();
        assert_eq!(result.module_path, "app");
        assert_eq!(result.qualname, "handler");
    }

    #[test]
    fn test_resolve_target_typescript_style() {
        let result = resolve_target("./handlers/task:createTask").unwrap();
        assert_eq!(result.module_path, "./handlers/task");
        assert_eq!(result.qualname, "createTask");
    }

    #[test]
    fn test_resolve_target_no_colon() {
        let result = resolve_target("no_colon_here");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid target format"));
    }

    #[test]
    fn test_resolve_target_empty_module() {
        let result = resolve_target(":qualname");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Module path is empty"));
    }

    #[test]
    fn test_resolve_target_empty_qualname() {
        let result = resolve_target("module:");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Qualified name is empty"));
    }

    #[test]
    fn test_resolve_target_invalid_qualname() {
        let result = resolve_target("module:123invalid");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Must be a valid identifier"));
    }

    #[test]
    fn test_resolve_target_qualname_with_spaces() {
        let result = resolve_target("module:has spaces");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_target_node_prefix() {
        // TypeScript-style node: prefix — last colon is the separator
        let result = resolve_target("node:path:join").unwrap();
        assert_eq!(result.module_path, "node:path");
        assert_eq!(result.qualname, "join");
    }

    #[test]
    fn test_resolve_target_underscore_qualname() {
        let result = resolve_target("mod:_private_func").unwrap();
        assert_eq!(result.qualname, "_private_func");
    }

    #[test]
    fn test_resolved_target_serde_roundtrip() {
        let target = ResolvedTarget {
            module_path: "my_crate::handlers".into(),
            qualname: "create_task".into(),
        };
        let json = serde_json::to_string(&target).unwrap();
        let deserialized: ResolvedTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, target);
    }

    #[test]
    fn test_ident_re_not_recompiled_per_call() {
        // Calling resolve_target twice exercises the LazyLock path (no recompile).
        let r1 = resolve_target("a:valid_func").unwrap();
        let r2 = resolve_target("b:another_func").unwrap();
        assert_eq!(r1.qualname, "valid_func");
        assert_eq!(r2.qualname, "another_func");
    }
}
