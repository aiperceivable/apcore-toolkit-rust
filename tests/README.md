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

This directory contains only test fixtures shared across modules:
- `fixtures/scanner_verb_map.json` — HTTP verb map test data

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
