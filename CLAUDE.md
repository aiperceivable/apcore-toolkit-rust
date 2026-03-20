# CLAUDE.md — apcore-toolkit-rust Development & Code Quality Specification

## Project Overview

`apcore-toolkit` is a **Rust port of apcore-toolkit-python** — shared scanner, schema extraction, and output toolkit for apcore framework adapters. It extracts framework-agnostic logic (scanning endpoints, extracting schemas, generating output formats) into a standalone, reusable crate.

**Reference implementation**: `../apcore-toolkit-python`

---

## Core Principles

- Prioritize **simplicity, readability, and maintainability** above all.
- Avoid premature abstraction, optimization, or over-engineering.
- Code should be understandable in ≤10 seconds; favor straightforward over clever.
- Always follow: **Understand → Plan → Implement minimally → Test/Validate → Commit**.

---

## Rust Code Quality

### Readability

- Use precise, full-word names; standard abbreviations only when idiomatic (`buf`, `cfg`, `ctx`).
- Functions ≤50 lines, single responsibility, verb-named (`parse_request`, `build_schema`).
- Avoid obscure tricks, overly chained iterators, unnecessary macros, or excessive generics.
- Break complex logic into small, well-named helper functions.

### Types (Mandatory)

- Provide explicit types on all public items; do not rely on inference for public API surfaces.
- Prefer `struct` over raw tuples for anything with more than 2 fields.
- Use **`enum`** for exhaustive variants; avoid stringly-typed logic.
- Implement `serde::Serialize` / `serde::Deserialize` on all public data types.

### Design

- Favor **composition over inheritance**; use `trait` only for true behavioral interfaces.
- Prefer plain functions + data structs; minimize trait object (`dyn Trait`) indirection.
- No circular module dependencies.
- Keep `pub` surface minimal — default to module-private, expose only what consumers need.

### Errors & Resources

- Define domain errors with **`thiserror`**; no bare `Box<dyn Error>` in library code.
- Propagate errors with `?`; no `unwrap()` / `expect()` in library paths (tests excepted).
- Validate and sanitize all public inputs at crate boundaries.

### Async

- Runtime: **Tokio** (`features = ["full"]`).
- Traits with async methods: use **`async-trait`**.

### Logging

- Use **`tracing`** — no `println!` / `eprintln!` in production code.

### Testing

- Run with: `cargo test --all-features`
- **Unit tests**: in the same file under `#[cfg(test)] mod tests { ... }`.
- Test names: `test_<unit>_<behavior>` (e.g., `test_filter_modules_include_pattern`).
- Never change production code without adding or updating corresponding tests.

### Serialization

- JSON: `serde_json`. YAML: `serde_yaml`.

---

## Mandatory Quality Gates

| Command | Purpose |
|---------|---------|
| `cargo fmt --all -- --check` | Formatting |
| `cargo clippy --all-targets --all-features -- -D warnings` | Lint |
| `cargo build --all-features` | Full build |
| `cargo test --all-features` | Tests |

---

## Module Map (ported from Python)

| Rust Module | Python Module | Purpose |
|-------------|---------------|---------|
| `types` | `types.py` | `ScannedModule` struct |
| `scanner` | `scanner.py` | `Scanner` trait (BaseScanner ABC) |
| `openapi` | `openapi.py` | OpenAPI $ref resolution & schema extraction |
| `schema_utils` | `schema_utils.py` | JSON Schema enrichment |
| `serializers` | `serializers.py` | ScannedModule serialization |
| `formatting/markdown` | `formatting/markdown.py` | Dict-to-Markdown conversion |
| `output/types` | `output/types.py` | Verifier trait, VerifyResult, WriteResult |
| `output/errors` | `output/errors.py` | WriteError |
| `output/yaml_writer` | `output/yaml_writer.py` | YAML binding file generation |
| `output/verifiers` | `output/verifiers.py` | Built-in verifier implementations |
| `output/registry_writer` | `output/registry_writer.py` | Direct registry registration |
| `output/http_proxy_writer` | `output/http_proxy_writer.py` | HTTP proxy module registration |
| `ai_enhancer` | `ai_enhancer.py` | SLM-based metadata enhancement |

## Dependency Management

- Evaluate necessity before adding a new dependency.
- Dev-only crates go in `[dev-dependencies]`, never `[dependencies]`.

## General Guidelines

- **English only** for all code, comments, doc comments, error messages, and commit messages.
- Fully understand surrounding code before making changes.
