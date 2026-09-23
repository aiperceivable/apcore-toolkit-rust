// Conformance harness: assert Rust's `BindingLoader` pattern matching matches
// the shared fixture corpus at
// `apcore-toolkit/conformance/fixtures/binding_pattern.json`.
//
// The Python and TypeScript SDKs run the same fixture file through their own
// `load(..., pattern=...)` implementations. This is the cross-SDK contract for
// the `bindings.pattern` argument (see
// `apcore-toolkit/docs/features/binding-loader.md#pattern-matching`).
//
// Two case kinds. There is no `validate` kind: as of 0.13.0 every string is a
// valid pattern and the loader never errors on one for a syntactic reason,
// matching apcore's Algorithm A25 requirement 2.
//
// - `match` — the pure name matcher, via `match_binding_pattern`.
// - `select` — how `pattern` composes with `recursive` over a real directory
//   tree, via the loader itself.
//
// Harness convention for `select` (stated in the fixture's own `description`):
// each entry of `input.files` is materialized under a temp root, creating
// parent directories as needed, so an entry that is a strict path prefix of
// another entry is a DIRECTORY rather than a file (case 034). Every file is
// written with a single binding whose `module_id` is its own `/`-separated
// path relative to the temp root, which lets the loader's returned modules be
// compared — in order — against `expected.selected`.
//
// A `select` case may also carry an `input.symlinks` map (link name → target,
// both relative to the temp root); targets are materialized first. Cases that
// need it are tagged `"requires": "symlinks"` and are SKIPPED with a visible
// message — never silently passed — if the platform refuses to create one.

use std::path::PathBuf;

use apcore_toolkit::{match_binding_pattern, BindingLoader};
use serde_json::Value;
use tempfile::TempDir;

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
}

fn load_cases() -> Vec<Value> {
    let path = conformance_dir().join("binding_pattern.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("WARN: conformance fixture not found at {path:?}; skipping");
            return Vec::new();
        }
    };
    let doc: Value = serde_json::from_str(&content).expect("fixture must be valid JSON");
    doc["test_cases"].as_array().cloned().unwrap_or_default()
}

/// A binding document whose single entry carries `module_id` = `rel_path`, so
/// the loader's output identifies which files it selected.
fn binding_document(rel_path: &str) -> String {
    let doc = serde_json::json!({
        "spec_version": "1.0",
        "bindings": [{"module_id": rel_path, "target": "fixture:noop"}],
    });
    serde_yaml_ng::to_string(&doc).expect("fixture document must serialize")
}

/// Materialize `files` under `root`, treating an entry that is a strict path
/// prefix of another entry as a directory rather than a file.
fn materialize(root: &std::path::Path, files: &[String]) {
    for entry in files {
        let is_directory = files
            .iter()
            .any(|other| other.starts_with(&format!("{entry}/")));
        let target = root.join(entry);
        if is_directory {
            std::fs::create_dir_all(&target).expect("create directory entry");
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent directories");
        }
        std::fs::write(&target, binding_document(entry)).expect("write binding file");
    }
}

/// Result of one fixture case. `Skip` exists so a platform that cannot create
/// symlinks reports the gap loudly instead of counting as a pass.
enum Outcome {
    Pass,
    Skip(String),
    Fail(String),
}

#[cfg(unix)]
fn make_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &std::path::Path, _link: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// Create `input.symlinks` under `root`. Targets must already exist, so this
/// runs after [`materialize`]. Returns the OS reason on failure so the case can
/// be skipped with a visible message.
fn create_symlinks(root: &std::path::Path, links: Option<&Value>) -> Result<(), String> {
    let Some(map) = links.and_then(Value::as_object) else {
        return Ok(());
    };
    for (link_name, target) in map {
        let target = target.as_str().expect("symlink target must be a string");
        if let Some(parent) = root.join(link_name).parent() {
            std::fs::create_dir_all(parent).expect("create symlink parent directories");
        }
        make_symlink(&root.join(target), &root.join(link_name))
            .map_err(|e| format!("cannot create symlink {link_name:?} -> {target:?}: {e}"))?;
    }
    Ok(())
}

/// The `module_id` declared by the binding file that `rel` names, following
/// symlinks just as the loader does.
fn module_id_at(root: &std::path::Path, rel: &str) -> String {
    let text = std::fs::read_to_string(root.join(rel))
        .unwrap_or_else(|e| panic!("expected selected path {rel:?} must be readable: {e}"));
    let doc: Value = serde_yaml_ng::from_str(&text).expect("expected file must be valid YAML");
    doc["bindings"][0]["module_id"]
        .as_str()
        .expect("expected file must declare a module_id")
        .to_string()
}

fn string_list(raw: Option<&Value>) -> Vec<String> {
    raw.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn run_match_case(input: &Value, expected: &Value) -> Outcome {
    let pattern = input["pattern"].as_str().expect("pattern must be a string");
    let name = input["name"].as_str().expect("name must be a string");
    let want = expected["matches"]
        .as_bool()
        .expect("matches must be a bool");

    let got = match_binding_pattern(pattern, name);
    if got == want {
        Outcome::Pass
    } else {
        Outcome::Fail(format!(
            "match({pattern:?}, {name:?}) expected {want}, got {got}"
        ))
    }
}

fn run_select_case(input: &Value, expected: &Value) -> Outcome {
    let pattern = input["pattern"].as_str().expect("pattern must be a string");
    let recursive = input["recursive"]
        .as_bool()
        .expect("recursive must be a bool");
    let files = string_list(input.get("files"));
    let want = string_list(expected.get("selected"));

    let root = TempDir::new().expect("temp dir");
    materialize(root.path(), &files);
    if let Err(reason) = create_symlinks(root.path(), input.get("symlinks")) {
        return Outcome::Skip(reason);
    }

    let modules = match BindingLoader::new().load_with_pattern(
        root.path(),
        false,
        recursive,
        Some(pattern),
    ) {
        Ok(modules) => modules,
        Err(e) => return Outcome::Fail(format!("expected Ok, got error: {e}")),
    };

    // The loader returns modules, not paths, so each expected path is resolved
    // to the `module_id` the loader must have produced from it — by reading the
    // file that path names, exactly as the loader does. For an ordinary file
    // that is the path itself; for a symlink it is the target's `module_id`,
    // since a symlink and its target are the same file and a pure-data loader
    // cannot observe which name it was reached by. Count and order still hold:
    // dropping the symlinked alias in case 040 yields one module, not two.
    let want_ids: Vec<String> = want
        .iter()
        .map(|rel| module_id_at(root.path(), rel))
        .collect();
    let got: Vec<String> = modules.iter().map(|m| m.module_id.clone()).collect();
    if got == want_ids {
        Outcome::Pass
    } else {
        Outcome::Fail(format!(
            "expected selected {want:?} (module_ids {want_ids:?}), got module_ids {got:?}"
        ))
    }
}

#[test]
fn binding_pattern_matches_shared_conformance_fixture() {
    let cases = load_cases();
    if cases.is_empty() {
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut passed: usize = 0;

    for case in &cases {
        let id = case["id"].as_str().unwrap_or("<unknown>");
        let description = case["description"].as_str().unwrap_or("");
        let kind = case["kind"].as_str().unwrap_or("<missing>");
        let input = &case["input"];
        let expected = &case["expected"];

        let outcome = match kind {
            "match" => run_match_case(input, expected),
            "select" => run_select_case(input, expected),
            other => Outcome::Fail(format!("unknown case kind {other:?}")),
        };

        match outcome {
            Outcome::Pass => passed += 1,
            Outcome::Skip(reason) => {
                let requires = case["requires"].as_str().unwrap_or("<unspecified>");
                skipped.push(format!("{id} (requires {requires}): {reason}"));
            }
            Outcome::Fail(reason) => {
                failures.push(format!("\nCase {id} ({kind}): {description}\n  {reason}"));
            }
        }
    }

    // Skips are printed, never silent: a platform that cannot create symlinks
    // must show the gap rather than counting those cases as passes.
    for skip in &skipped {
        eprintln!("SKIP {skip}");
    }
    eprintln!(
        "binding_pattern: {} passed, {} skipped, {} failed, of {} shared conformance case(s)",
        passed,
        skipped.len(),
        failures.len(),
        cases.len()
    );

    assert!(
        failures.is_empty(),
        "{} of {} case(s) failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    assert_eq!(
        passed + skipped.len(),
        cases.len(),
        "every fixture case must be run or explicitly skipped"
    );
}
