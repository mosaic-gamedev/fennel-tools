# Performance Considerations

This document surveys the hot paths in `fennel-ls-rs` and ranks potential
optimisations from lowest to highest implementation cost. The server is fast
enough for typical Fennel files today (full pipeline < 1 ms for < 5000 lines),
but some of these become important at scale or when cross-file analysis lands.

---

## Already fast (no action needed)

- **Single-pass analyzer.** The entire scope/reference/warning pass is one
  recursive traversal. No separate "resolve" or "typecheck" phases.
- **Byte-oriented lexer.** Operates on `&[u8]`, avoiding UTF-8 decode overhead
  during tokenisation. Only position reporting converts to chars.
- **Sorted `syms` vec.** `symbol_at` uses binary search + short backwards walk,
  not a linear scan. `build_semantic_tokens` emits deltas in one forward pass.
- **`DashMap` for open files.** Sharded concurrent hashmap; multiple handlers
  can read different files simultaneously without contention.

---

## Low-hanging fruit

### Line index for position conversion
`position_to_byte` and `byte_to_position` in `text.rs` both scan the full text
linearly to count newlines. For a 10 000-line file every hover, definition, or
completion request pays O(n) just to convert the cursor position.

**Fix:** cache a `Vec<u32>` of newline byte offsets alongside the text in
`AnalyzedFile`. Line→byte becomes an index lookup; byte→line becomes
`binary_search`. The cache is built once per `update()` call and invalidated
automatically when the file changes.

### Semantic token caching
`semantic_tokens_full` rebuilds the token list on every request by iterating
all syms. The result is deterministic for a given analysis — there is no reason
to recompute it unless the file has changed.

**Fix:** store `Option<Vec<SemanticToken>>` in `AnalyzedFile`, populated on
the first request and cleared in `Workspace::update()`.

### Debounce rapid keystrokes
With incremental sync the pipeline re-runs on every `didChange` event. A user
typing quickly sends one event per character; most intermediate states are
thrown away immediately.

**Fix:** buffer incoming changes for ~50 ms before triggering analysis. Use a
`tokio::time::sleep` + cancellation token pattern — cancel the pending sleep on
each new change, restart it. Only the final state in the burst gets analyzed.
Latency impact on diagnostics is imperceptible; CPU usage on large files drops
proportionally to typing speed.

### Move analysis off the async runtime
`Workspace::update()` runs the full lexer → parser → analyzer chain
synchronously on the tower-lsp handler task. This blocks the async runtime
during analysis, delaying other LSP responses (e.g. hover requests that arrive
while a large file is being analyzed).

**Fix:** wrap `update()` in `tokio::task::spawn_blocking`. The handler `await`s
the join handle; the runtime thread pool handles the CPU work. This is a
one-line change at each `workspace.update(...)` call site.

---

## Medium effort

### Cached completion candidates
`completion` rebuilds the full item list (scope defs + all builtins) on every
trigger. The builtin portion is static for the lifetime of the server.

**Fix:** precompute `Vec<CompletionItem>` for the active `BuiltinSet` once in
`Workspace::configure_platform` / `with_builtins` and reuse it. Only the
scope-local portion (which changes per cursor position) needs to be rebuilt
per-request.

### String interning for symbol names
Every `SymbolEntry`, `DefinitionInfo`, and `Scope` binding stores a `String`.
A file with 1000 definitions allocates 1000 strings; a file with 5000 references
allocates 5000 more, even though the vast majority are copies of the same ~50
names.

**Fix:** introduce a per-file string interner (`HashMap<&str, u32>` + `Vec<Box<str>>`
arena). Replace `name: String` with `name: u32` (an intern index). Equality
checks become integer comparisons. The interner lives in `AnalyzedFile` and is
rebuilt on each `update()`. The crate `lasso` provides a battle-tested
implementation.

### Interval tree for `symbol_at`
`symbol_at` binary-searches by `span.start` then walks backwards to find the
narrowest covering span. In a file with thousands of symbols and many nested
forms this walk can be long.

**Fix:** build an interval tree (e.g. `rust-lapper`) over `syms` spans during
analysis. Point queries become O(log n + k) where k is the number of
overlapping intervals at the cursor — typically 1–3 for well-structured code.

### Avoid re-hashing definition keys
`AnalysisResult::defs` is `HashMap<u32, DefinitionInfo>` keyed by byte offset.
Lookups from `symbol_at` results are frequent. Since byte offsets are already
integers, switching to `FxHashMap` (from `rustc-hash`) or `AHashMap` drops
the hash cost to near-zero with no API change.

---

## Cross-file analysis (future, high impact)

These only matter once `(require :mod)` targets are loaded, but designing them
correctly from the start avoids expensive rewrites later.

### Dependency graph with BFS invalidation
A naïve implementation re-analyzes all files when any file changes. With a
directed dependency graph (`requires_map: HashMap<FileId, HashSet<FileId>>`),
invalidation visits only the reverse-transitive closure of the changed file —
typically O(1) for leaf modules, O(n) only for shared utility files.

### Exports index
Cross-file completion and hover need to know what names a module exports without
re-analyzing it on every request. An `exports: HashMap<FileId, Vec<DefinitionInfo>>`
(keyed by file, populated on first analysis, invalidated when the file changes)
separates the "what does this module export" question from the "analyze this
call site" question. Re-analysis of an importer only needs to re-read the
index, not re-parse the dependency.

### Lazy dependency loading
Load required modules on demand (first hover/definition/completion that needs
them) rather than eagerly at `didOpen`. Most users only interact with a small
fraction of transitively required files during a session.

### Per-file analysis result versioning
Tag each `AnalyzedFile` with a monotonic version counter. Cross-file consumers
store the version they last read from each dependency. On a request, compare
stored vs. current version: if equal, the cached cross-file result is still
valid. This eliminates redundant re-resolution when an unrelated file changes.

---

## Profiling guidance

Before implementing any of the above, profile first:

```bash
# Build with debug symbols in release mode
cargo build --release --config 'profile.release.debug=true'

# Use samply (macOS/Linux) or cargo-flamegraph
cargo install samply
samply record ./target/release/fennel-ls
# Then open a Fennel file in your editor and exercise the slow operation
```

The most likely bottleneck for a typical session is **position conversion**
(called on every keystroke-triggered handler). The most likely bottleneck at
scale (> 50 open files) will be **cross-file invalidation** once that feature
exists.
