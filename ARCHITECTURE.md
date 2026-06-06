# Chotu Project Architecture & Engineering Guidelines

This document outlines the architectural standards, Rust best practices, and safety guidelines for **Project Chotu**. As a developer learning Rust, these principles serve as both codebase standards and educational reference points.

---

## 1. Safety & The "Zero-Unsafe" Policy

* **Strict Safe Rust**: We enforce a strict **zero-`unsafe`** code policy. No `unsafe` blocks are allowed in the source code of any crate in this workspace.
* **FFI & External Boundaries**: If a dependency or OS-level feature requires `unsafe` code (e.g., interfacing with a C library), it must be:
  1. Flagged explicitly in an architectural review.
  2. Wrapped in a completely safe, idiomatic Rust API boundary in `chotu-common`.
  3. Documented with a `// SAFETY:` comment explaining why the invariants cannot be violated.

---

## 2. Asynchronous Tokio & Concurrency Patterns

* **Task Decoupling**: Each agent runs as an independent Tokio task or binary process. They communicate either via the database (persistent state) or via message-passing channels (in-memory IPC).
* **Non-Blocking Execution**: Never call blocking OS operations (like `std::fs` or `std::thread::sleep`) inside an async context. Instead, use their Tokio equivalents (`tokio::fs` or `tokio::time::sleep`). If a blocking call is unavoidable (e.g. standard CSV parsing), wrap it in `tokio::task::spawn_blocking`.
* **Locking Minimization**:
  * Prefer Tokio channels (`tokio::sync::mpsc` or `broadcast`) over sharing state via `Arc<Mutex<T>>`.
  * If a `Mutex` is necessary, use standard `std::sync::Mutex` if the lock is **never** held across an `.await` boundary. Use `tokio::sync::Mutex` *only* if the lock must be held across yield points, to avoid blocking the runtime executor thread.

---

## 3. Idiomatic Error Handling

We distinguish between **application binaries** (agents) and **libraries** (`chotu-common`):

### In `chotu-common` (Library Crate)
* Do **not** use `anyhow` for errors that callers might need to inspect and handle.
* Instead, define semantic, strongly-typed error enums using the `thiserror` crate (or custom implementations of the `std::error::Error` trait). This allows calling agents to match on specific error kinds.
* Avoid `.unwrap()` or `.expect()`. Use the `?` operator or map errors explicitly.

### In Agent Binaries (`streamer`, `janitor`, `coordinator`)
* Use the `anyhow` crate for high-level error propagation and context.
* Add context to errors using `context("...")` to produce human-readable backtraces when an agent fails.

---

## 4. Ownership, Lifetimes, and API Design

* **Prefer Owned Data in Tasks**: To satisfy Tokio's `'static` requirement for spawned tasks, favor moving owned data (`String`, `PathBuf`) or reference-counted pointers (`Arc<T>`) rather than dealing with complex lifetime parameters (`'a`).
* **Newtype Pattern**: Use Rust's newtype pattern to enforce compile-time validation (e.g., `struct EmailAddress(String);` or `struct TransactionAmount(Decimal);`). This prevents passing incorrect data formats to functions.
* **Compile-Time SQL Validation**: Leverage `sqlx`'s type checking to catch schema-query mismatches at compilation time.
