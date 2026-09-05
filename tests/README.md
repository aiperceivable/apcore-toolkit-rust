# Tests

Rust tests for apcore-toolkit live **inline** inside each `src/*.rs` module,
inside `#[cfg(test)] mod tests { ... }` blocks. This is idiomatic Rust and allows
tests to access private helpers directly without `pub(crate)` widening.

See examples:
- `src/scanner.rs` — `filter_modules`, `deduplicate_ids`, `infer_annotations_from_method`
- `src/binding_loader.rs` — `BindingLoader` integration tests (uses temp directories)
- `src/output/yaml_writer.rs` — `YAMLWriter` write/verify/dry-run tests
- `src/output/registry_writer.rs` — `RegistryWriter` registration tests
- `src/display/resolver.rs` — `DisplayResolver` resolution tests

This directory contains black-box integration tests that exercise the
public API from outside the crate (mirroring the per-module test files
present in the Python and TypeScript SDKs), plus shared fixtures:

- `public_api_integration.rs` — exercises a representative sample of
  crate-root re-exports (`VERSION`, `get_writer`, `resolve_http_verb`,
  path-param helpers, `InvalidFormatError`, `OutputFormat`, etc.), not an
  exhaustive list — most public symbols are covered by inline unit tests
  colocated with their source files instead.
- `scanner_verb_map_conformance.rs` — runs every case in the shared
  conformance fixture against `generate_suggested_alias`.
- `annotation_conformance.rs` — locks the `RegistryWriter` scan → register →
  `get_definition` round-trip that approval/ACL gating depends on, via
  `assert_annotations_preserved` against a real `apcore::Registry`.
- `display_resolve_conformance.rs` — cross-SDK conformance harness for
  `DisplayResolver` against the shared `display_resolve.json` corpus.
- `openapi_scan_conformance.rs` — cross-SDK conformance harness asserting
  `OpenAPIScanner` matches the shared `openapi_scan.json` corpus.
- `tabular_conformance.rs` — cross-SDK conformance harness asserting
  `format_csv` / `format_jsonl` produce byte-identical output against the
  shared tabular fixture corpus.
- `view_model_conformance.rs` — cross-SDK conformance harness asserting
  `modules_to_view_model` / `format_view_model` produce byte-identical
  output against the shared `view_model.json` corpus.
- `fixtures/scanner_verb_map.json` — HTTP verb map test data (shared with
  the Python and TypeScript SDKs).

## Running tests

```sh
# All tests
cargo test

# With output visible
cargo test -- --nocapture

# Specific test
cargo test binding_loader::tests::test_load_strict_mode

# All tests in a module
cargo test output::yaml_writer::tests
```

## Note on doctest `#[ignore]`

Some doctests are marked `#[ignore]` (e.g., `BindingLoader`, `DisplayResolver`) because
they require filesystem state that cannot be set up in a doctest context. These examples
are still valid documentation; their behavior is covered by the inline unit tests above.
Convert them to runnable doctests or explain the ignore reason if making changes.
