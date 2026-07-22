# fennel-ls-rs — Architecture Reference

A language server for [Fennel](https://fennel-lang.org/) written in Rust. Fennel is a Lisp dialect
that compiles to Lua. The server speaks LSP over stdin/stdout.

---

## Project layout

```
fennel-ls-rs/
├── src/
│   ├── main.rs        — CLI entry point (server mode + check mode)
│   ├── server.rs      — LSP handler (tower-lsp Backend)
│   ├── workspace.rs   — Open-file registry (DashMap)
│   ├── lexer.rs       — Fennel tokenizer
│   ├── parser.rs      — Token → AST (recursive descent)
│   ├── analyzer.rs    — Single-pass scope + reference analysis
│   ├── docs.rs        — Built-in documentation table
│   └── text.rs        — UTF-16 ↔ byte-offset conversion
├── tree-sitter-fennel/ — Forked tree-sitter grammar (separate concern)
├── flake.nix          — Nix dev shell (rustup + nodejs + tree-sitter CLI)
└── .envrc             — direnv hook (`use flake`)
```

The root `Cargo.toml` is a **single crate** (`fennel-ls`). The
`tree-sitter-fennel/Cargo.toml` exists for the upstream crate but is not
included in any workspace — the grammar lives here purely as source to
maintain and regenerate; it is not linked into the LSP binary.

---

## Build and run

```bash
# Standard Rust build
cargo build --release

# Run the language server (reads LSP from stdin, writes to stdout)
./target/release/fennel-ls

# Lint files from the command line (no LSP client needed)
./target/release/fennel-ls check file.fnl
```

The `check` subcommand prints parse errors and unknown-identifier warnings
in `file:line:col: severity: message` format and exits 1 if any errors were
found.

---

## LSP features

All features are scoped to the currently open file — there is no cross-file
analysis yet.

| Capability | Behaviour |
|---|---|
| **Diagnostics** | Parse errors (ERROR) + semantic warnings (WARNING): unknown identifier, immutable-set, unused `var`, unused local, unused param, arity mismatch. Pushed on every `didOpen`/`didChange`/`didSave`. |
| **Hover** | For user-defined names: shows `(kind name [params])` signature + inline docstring if the function body begins with a string literal. For built-in names: shows the signature and description from `docs.rs`. |
| **Go to definition** | Jumps to the binding site of the symbol under the cursor. When the cursor is on a `require`/`import-macros` module string, resolves and jumps to the `.fnl` file on disk. |
| **Find references** | Returns all use-sites of the definition the cursor is on (or the definition a reference points to). Includes the definition itself. |
| **Document highlight** | Same logic as find-references but returns `WRITE` kind for the definition and `READ` for uses. |
| **Document symbols** | Lists every definition in the file (all `DefKind` variants). |
| **Completion** | Returns scope-local definitions (respecting lexical scope at the cursor position) followed by all built-ins and any custom `global_docs` entries. Deduplicates by name. Sorted alphabetically. |
| **Rename** | Renames all occurrences of a definition within the current file. |
| **Code actions** | `var → local` quickfix on any `var` that was never mutated. Unknown-identifier quickfix that inserts `(local name nil)` above the offending line. |
| **Semantic tokens** | Full-document classification (`function`, `parameter`, `variable`, `macro`). Modifiers: `definition`, `readonly`. Delta-encoded from the sorted `syms` vec. |

Trigger characters for completion: `(`, `[`, `{`, `.`, `:`.

Text sync mode: **INCREMENTAL** — the client sends only changed ranges; each
`TextDocumentContentChangeEvent` is applied in sequence to the in-memory text,
then the pipeline re-runs on the full (now-updated) text.

**Configuration:** on `initialize`, the server reads `.fennel-ls.toml` from
the workspace root (if present). See the **Configuration** section below.

---

## Configuration

The server looks for `.fennel-ls.toml` in the workspace root on startup.
All fields are optional; an empty file or a missing file is valid.

### Fields

| Field | Type | Description |
|---|---|---|
| `platform` | string | Lua platform for built-in docs: `"lua51"`, `"lua52"`, `"lua53"`, `"lua54"` (default), `"luajit"`, `"luau"`. |
| `known_globals` | string array | Global names that suppress unknown-identifier warnings but have no hover documentation. Roots derived from `global_docs` keys are added automatically, so only list globals with no associated docs here (e.g. engine-injected tables like `state`). |
| `include` | string array | Paths (relative to the workspace root) of extra TOML files whose `[global_docs]` sections are merged into this config. Use this to keep engine/framework API docs in one shared file and reference them from many per-project configs. |
| `global_docs` | table | Per-symbol hover documentation. See below. |

### `global_docs`

Each key is the exact Fennel symbol as it appears in source code, including
dots for namespaced APIs.  Each value has two sub-fields:

| Sub-field | Required | Description |
|---|---|---|
| `signature` | yes | Short Fennel-style call form shown in the hover code block. |
| `doc` | no | Prose description shown below the signature. Supports Markdown. |

**Root inference:** the server automatically extracts the root of every
`global_docs` key (everything before the first `.` or `:`) and adds it to the
known-globals set.  You do not need to list `Mosaic` in `known_globals` just
because you have `global_docs."Mosaic.Grid.set_tile"`.

**Hover fallback:** on hover the server tries the full symbol name first
(e.g. `Mosaic.Grid.set_tile`), then strips the last member and retries
(`Mosaic.Grid`), then strips again (`Mosaic`), until it finds a match or
exhausts the chain.  A single entry for a namespace therefore acts as a
fallback doc for any undocumented member of that namespace.

### Minimal example

```toml
platform = "luajit"
known_globals = ["state"]          # persistent game-state table, no docs needed
include = ["../../engine/api.toml"] # engine API docs live in the engine repo
```

### Splitting docs into a shared file

Put the `[global_docs]` table in a separate TOML file (e.g. `mosaic.toml`)
alongside the engine source, then reference it from each project's
`.fennel-ls.toml`.  The included file may only contain a `[global_docs]`
section; all other fields are ignored.

**`engine/mosaic.toml`:**
```toml
[global_docs."Mosaic.Grid.set_tile"]
signature = "(Mosaic.Grid.set_tile col row index primary secondary rotation)"
doc = """
Draw a tile on the grid.
- `primary` / `secondary` — `{r g b a}` colour tables (values 0–1).
- `rotation` — radians clockwise around the tile centre (default `0`).
"""

[global_docs."Mosaic.Input.cursor_cell"]
signature = "(Mosaic.Input.cursor_cell)"
doc = "Returns `{col row}` of the cell under the cursor, or `nil` if the cursor is outside the grid."
```

**`my-game/.fennel-ls.toml`:**
```toml
platform = "luajit"
known_globals = ["state"]
include = ["../engine/mosaic.toml"]
```

---

## Pipeline (per keystroke)

```
source text
  └─► Lexer::tokenize()          [lexer.rs]       Vec<SpannedToken>
        └─► Parser::parse()      [parser.rs]      (Vec<AstNode>, Vec<ParseError>)
              └─► analyze()      [analyzer.rs]    AnalysisResult
                    │
                    ├─ defs:   HashMap<byte_offset, DefinitionInfo>
                    ├─ refs:   HashMap<ref_byte, def_byte>
                    ├─ syms:   Vec<SymbolEntry>  (sorted, for binary search)
                    └─ scopes: Vec<Scope>        (tree, for completion)
```

This runs synchronously inside `Workspace::update()` on every `didChange`.
For typical Fennel files (<5000 lines) the full pipeline takes well under
1 ms.

---

## Modules in detail

### `lexer.rs`

Hand-written byte-oriented tokenizer. Operates on `&[u8]` for speed.

**Token types:** `LParen`, `RParen`, `LBrace`, `RBrace`, `LBracket`,
`RBracket`, `Quote`, `Quasiquote`, `Unquote`, `UnquoteSplice`, `HashFn`,
`Str(String)`, `Keyword(String)`, `Number(f64)`, `Bool(bool)`, `Nil`,
`Varargs`, `Symbol(String)`.

**Comma handling:** Commas are whitespace separators in Fennel. The lexer
peeks one character ahead:
- `,@…` → `Token::UnquoteSplice`
- `,<non-ws non-delim>` → `Token::Unquote`
- `,<ws|delim|eof>` → silently skipped (separator)

**String escapes** (full Lua 5.3 set):
`\a \b \f \n \r \t \v \\` `\"` `\'`, `\<newline>` (line continuation),
`\NNN` (1–3 decimal digits, byte value), `\xHH` (two hex digits),
`\u{HHHH…}` (Unicode code point), `\z` (skip following whitespace).

**Numbers:** Hex integers (`0x…`/`0X…`) parsed via `u64::from_str_radix`;
hex floats (`0x1.8p+1`) parsed via `parse_hex_float`; `.inf`/`+.inf`/`-.inf`
and `.nan`/`+.nan`/`-.nan` parsed as `f64::INFINITY`/`f64::NAN`; all other
floats parsed via `f64::from_str` after stripping `_` separators.

> **Note on `nan`/`-nan`:** Fennel's `parse-number` (`src/fennel/parser.fnl`)
> explicitly rejects bare `nan` and `-nan` as number literals (they become
> symbols). Our lexer delegates to `f64::parse` which accepts `"nan"` → `NaN`.
> This is a minor divergence; it produces false-negatives (missed
> "undefined identifier" warnings) rather than false-positives.

**Shebang:** A `#!` on byte 0 of the file is consumed as a comment line.

**Spans:** Every `SpannedToken` carries `(start, end, line, col, end_line, end_col)`,
all byte-based. Line and column are 0-based. `col` is a byte column (not
UTF-16). LSP position conversion is handled separately in `text.rs`.

### `parser.rs`

Recursive-descent parser over `Vec<SpannedToken>`.

**AST node (`Form` enum):**
- Atoms: `Symbol(String)`, `Keyword(String)`, `Str(String)`, `Number(f64)`,
  `Bool(bool)`, `Nil`, `Varargs`
- Compound: `List(Vec<AstNode>)` `(…)`, `Table(Vec<AstNode>)` `{…}`,
  `Sequence(Vec<AstNode>)` `[…]`
- Reader macros: `Quote`, `Quasiquote`, `Unquote`, `UnquoteSplice`, `HashFn`
  (each wraps a `Box<AstNode>`)

`List`, `Table`, and `Sequence` are flat — the parser does not interpret
Fennel semantics (that is the analyzer's job). `{:a 1 :b 2}` becomes
`Table([Keyword("a"), Number(1), Keyword("b"), Number(2)])`.

**Error recovery:** Unclosed delimiters emit a `ParseError` and return the
partial form. Unexpected closing delimiters are skipped with an error.
The parser always returns as many valid top-level forms as possible.

**Span merging:** Compound forms span from the opening delimiter's `start`
to the closing delimiter's `end`. Reader macro spans cover the macro
character through the end of the inner expression.

Helper `head_sym(forms)` returns the first element as `&str` if it is a
`Symbol`, used throughout the analyzer for dispatch.

### `analyzer.rs`

Single recursive pass over the AST. Builds all four result structures
simultaneously.

**`DefKind` variants:** `Local`, `Var`, `Global`, `Fn`, `Macro`, `Param`,
`LoopVar`, `Destructured`.

**`DefinitionInfo`** stores name, kind, span, optional param names (for
functions), optional inline docstring, and a `variadic: bool` flag set to
`true` when the param list contains `&` or `...` (`Form::Varargs`).

**`SymbolEntry`** represents every occurrence of a symbol (both definitions
and references). The `def_byte` field links a reference to its definition's
byte offset; `is_def` distinguishes the two.

**Scope tree:** Each scope records its span and a `HashMap<name, def_byte>`
of local bindings, plus a parent index. `push_scope`/`pop_scope` maintain a
stack. `defs_at(byte)` walks the scope chain outward from the innermost
scope that contains `byte` — used for completion.

**`symbol_at(byte)`** binary-searches `syms` (sorted by `span.start`) then
walks backwards to find the narrowest covering span — used for hover,
go-to-definition, rename, etc.

**Form dispatch in `analyze_list`** matches on `head_sym` to call the
appropriate handler:

| Head symbol(s) | Handler | What it does |
|---|---|---|
| `local` `var` `global` | `analyze_binding` | Evaluates RHS before binding LHS pattern |
| `set` | `analyze_set` | Records target as a reference; evaluates RHS |
| `fn` | `analyze_fn` | Binds optional name, opens scope, binds params, records docstring, analyzes body |
| `lambda` `λ` | `analyze_lambda` | Delegates to `analyze_fn` |
| `let` | `analyze_let` | Sequential pairs: RHS evaluated then LHS bound |
| `do` | `analyze_do` | Opens a scope; walks body |
| `if` | `analyze_if` | No scope; walks all sub-forms |
| `when` `unless` | — | No scope; walks sub-forms |
| `while` | `analyze_while` | No scope; walks sub-forms |
| `each` | `analyze_each` | Scope for loop vars; detects `&until` boundary |
| `for` | `analyze_for` | Scope; binds numeric loop variable |
| `macro` | `analyze_macro_def` | Delegates to `analyze_fn` (body fully analyzed), then re-tags `DefKind` as `Macro` |
| `macros` | `analyze_macros_form` | Binds each name from the table |
| `import-macros` | `analyze_import_macros` | Binds imported macro names |
| `match` `case` | `analyze_match_form` | Per-arm scope with pattern binding |
| `case-try` `match-try` | `analyze_case_try` | Shared scope for normal arms; per-arm scopes for catch |
| `collect` `icollect` | `analyze_collect` | Detects `&into` and `&until` boundaries; binds loop vars |
| `fcollect` | `analyze_fcollect` | Like `for` with optional `&into` |
| `accumulate` `faccumulate` | `analyze_accumulate` | Binds accumulator + loop vars; detects `&until` |
| `with-open` | `analyze_with_open` | Sequential pairs like `let` |
| anything else | `analyze_forms` | Walk head + all args |

**Quasiquotes:** `analyze_quasiquote` recurses into quasiquoted structures,
only actually analyzing expressions that appear inside `Unquote` or
`UnquoteSplice` forms.

**`HashFn`** opens a scope and pre-defines `$`, `$1`–`$9` as implicit
parameters.

**`bind_pattern`** handles destructuring recursively:
- `Symbol` → define (ignoring `_` and `&`)
- `Sequence` → sequential destructuring `[a b c]`, `[a & rest]`
- `List` → multi-value destructuring `(a b)`
- `Table` → table destructuring: `{:key name}`, `{"key" name}`, `{: name}`,
  `{name :key}`, `{&as whole}`, nested patterns

**`bind_match_pattern`** is a variant for `match`/`case` patterns where
plain symbols are bindings but `_`-prefixed names are discards, and
`(where pattern guard)` wraps a sub-pattern with a guard expression.

**Semantic warnings** are accumulated in `AnalysisResult::warnings`
(`Vec<AnalysisWarning>`, where each entry has a `message` and `span`).
Rules implemented:
- **Immutable set:** `analyze_set` warns when the target resolves to a
  `DefKind::Local` or `DefKind::Fn` binding.
- **Unused var:** `analyze_binding` warns when a `var` binding is never
  reassigned via `set`.
- **Arity mismatch:** `check_arity` is called from the `_` (catch-all) branch
  of `analyze_list`. It resolves the call head to a `DefKind::Fn` with a
  known `params` list and warns if the argument count falls outside the
  expected range. Skipped when `variadic: true`.
- **Unused locals:** Post-analysis pass over all `DefKind::Local` defs;
  warns on any def not present in the `refs` value set, unless the name
  starts with `_`.
- **Unused params:** Same pass over `DefKind::Param` defs; additionally
  skips synthesised names that start with `<` (e.g. destructured captures).

### `docs.rs`

Configurable builtin documentation with platform selection.

**`BuiltinDoc`** holds `signature: String`, `doc: String`, and `BuiltinKind`
(`Function`, `SpecialForm`, `Macro`, `Value`).

**`Platform`** enum: `Lua51`, `Lua52`, `Lua53`, `Lua54` (default), `LuaJIT`,
`Luau`. Selects the set of available globals (e.g. `unpack` is top-level in
Lua 5.1, `table.unpack` in 5.4; `ffi`/`jit`/`bit` only in LuaJIT).

**`BuiltinSet`** is the runtime doc table for a given platform:
- `BuiltinSet::for_platform(p)` — builds the set from a shared common map
  plus platform-specific extras.
- `with_extra(iter)` — extends the set with user-supplied entries (e.g.
  `love`, `vim`, `hs`).
- `get(name)` — exact lookup then multisym fallback (strips `.`/`:` suffix).
- `is_known(name)` — true if root is in the set.
- `iter()` — yields all entries for completion.

`default_set()` returns a `'static`-lifetime `BuiltinSet` for Lua 5.4 via
`OnceLock`, used by `run_check` in `main.rs`.

`known_global(name)` in `server.rs` is a separate allowlist for common Lua
globals that don't need documentation (`_G`, `_VERSION`, `vim`, `love`,
`jit`, `ffi`, `hs`, `mp`, …).

### `text.rs`

LSP positions are `(line, character)` where `character` is a UTF-16 code
unit offset. Internally all positions are byte offsets into the UTF-8 source.

- `position_to_byte(text, pos)` → byte offset (for all LSP requests that
  take a cursor position)
- `byte_to_position(text, byte)` → LSP Position (for building Location
  responses)
- `span_to_range(text, span)` → LSP Range (wraps two `byte_to_position`
  calls)

Emoji and other characters outside the BMP (code points ≥ U+10000) encode
as two UTF-16 code units but one Rust `char`, so the conversion counts
`char::len_utf16()` rather than `char::len_utf8()`.

### `workspace.rs`

`Workspace` wraps a `DashMap<String, AnalyzedFile>` keyed on the URI string.
`DashMap` provides concurrent read access without a global lock — relevant
because tower-lsp can call handlers from multiple async tasks.

`update(uri, text, version)` runs the full lexer → parser → analyzer
pipeline and stores the result. `remove(uri)` drops the entry. `with_file`
takes a closure so the DashMap entry is dropped as quickly as possible.

### `server.rs`

Implements `tower_lsp::LanguageServer` for `Backend { client, workspace }`.

`publish_diagnostics` is called after every text sync event. It emits three
kinds of diagnostics, then calls `client.publish_diagnostics`:

1. **Parse errors** — `ERROR` nodes in the AST, severity `Error`.
2. **Semantic warnings** — entries in `AnalysisResult::warnings`, severity
   `Warning` (e.g. `set` on an immutable binding).
3. **Unknown identifiers** — unresolved symbol references, severity `Warning`.

Unknown identifiers: a `SymbolEntry` is a warning candidate when
`is_def == false` AND `def_byte == None` AND `docs::get(name) == None` AND
the root name is not in `known_global`. Multi-symbol names like `a.b.c` are
checked via their root (`a`).

---

## Known limitations and proposals

### Cross-file analysis

**Current behaviour:** Every file is analyzed in isolation. `(require :mod)`
and `(import-macros {: f} :mod)` calls are recognized syntactically but their
targets are never loaded, so symbols imported from other modules appear as
unknown identifiers.

**Proposed fix:**

1. **Module resolution.** When a file is parsed, scan its top-level forms for
   `require` and `import-macros` calls, extract the module path string, and
   resolve it to a file URI using Fennel's standard search conventions
   (`?.fnl`, `?/init.fnl`) relative to the workspace root. The client sends
   the workspace root folder in `InitializeParams`; store it in `Backend`.

2. **Dependency graph in `Workspace`.** Add a `deps: DashMap<String,
   Vec<String>>` (file URI → list of required URIs) alongside `files`. When
   `update()` is called, compute the new dep list, compare to the old one, and
   load any newly required files that are not already tracked.

3. **Exported name index.** For each analyzed file add an `exports:
   HashMap<String, u32>` (name → def_byte) field to `AnalyzedFile` that lists
   top-level `global` and `local`/`fn` definitions whose names don't start
   with `-` or `_` (Fennel's privacy convention). When resolving a `require`,
   look up the required file's exports and inject them as definitions in the
   requiring file's global scope.

4. **Invalidation.** When a required file changes, re-analyze all files that
   depend on it. A simple BFS over the dependency graph is sufficient; cycles
   need detection (track a `visited` set).

   Workspace symbols (`workspace/symbol`) fall out of this naturally: search
   `exports` across all cached `AnalyzedFile` entries.

### Semantic correctness (additional lint rules)

**Current behaviour:** `set`-on-immutable is implemented (see analyzer section).
The following are not yet implemented:

- **Unused `var`.** After analysis, find all `DefKind::Var` entries that have
  no reference with a later `span.start`. Emit a hint-level diagnostic
  suggesting `local` instead. This is purely additive to the existing
  `AnalysisResult`.

- **Shadowed bindings.** In `define()`, check whether the name already exists
  in the *current* scope (not a parent scope). Same-scope shadowing is always
  a mistake in Fennel; parent-scope shadowing is intentional and should not
  warn.

- **Call-site arity.** When `analyze_forms` sees a `List` whose head resolves
  to a `DefKind::Fn` entry with a known `params` list, count the arguments and
  warn if the count is wrong (accounting for `...` varargs).

### Macro expansion (call-site bindings)

**Current behaviour:** `analyze_macro_def` delegates to `analyze_fn`, so
hover/go-to-def works for names *inside* a macro definition. What's missing is
synthesizing the names a macro *introduces* at its call sites.

**Proposed fix:** At a `(some-macro ...)` call site the macro may introduce new
names into the caller's scope (hygienic or not). Properly handling this
requires either:

- **Static template expansion:** For simple macros whose body is a quasiquote
  that expands to `(local ,name ,val)` or similar, pattern-match the body at
  the AST level to predict what names would be bound. This works for a useful
  subset of real-world macros.
- **Fennel subprocess:** Spawn `fennel --eval` (if available on `PATH`) to
  expand a macro call and parse the result. Cache expansions keyed on the
  macro's source span + arguments. This is the only way to handle arbitrary
  macros correctly but introduces an external dependency.

The subprocess approach is the right long-term answer. A feature flag in
`Backend` (populated from LSP initialization options) could enable it so users
without Fennel installed still get a working (if limited) server.

### Workspace symbols

**Current behaviour:** `workspace/symbol` is not implemented.

**Proposed fix:** This is a direct consequence of cross-file analysis. Once the
dependency graph and exports index described above are in place:

1. Implement `LanguageServer::workspace_symbol` in `server.rs`.
2. Iterate over all `AnalyzedFile` entries in the `DashMap`.
3. For each file, scan `analysis.defs` for entries whose name contains the
   query string (case-insensitive substring match is sufficient).
4. Map each match to a `SymbolInformation` with the file's URI.

Even without cross-file analysis this can be wired up for single-file
workspaces by searching across all *open* files, which is a useful interim
state.

### Semantic tokens

**Current behaviour:** Not implemented. Editors fall back to their built-in
tree-sitter / regex highlighting.

**Proposed fix:**

1. Declare support in `initialize`: add `SemanticTokensOptions` with token
   types `["namespace", "function", "variable", "keyword", "string",
   "number", "operator", "macro"]` and no modifiers (keep it simple).

2. Implement `textDocument/semanticTokens/full` by walking `AnalysisResult`:
   - `syms` entries with `is_def = true` and `DefKind::Fn` → `function`
   - `syms` entries with `is_def = true` and `DefKind::Macro` → `macro`
   - `syms` entries with `is_def = false` and `def_byte` pointing to a `Fn`
     def → `function`
   - Built-in special forms (looked up via `docs::get`) → `keyword`
   - `syms` entries that are `DefKind::Local`/`Var`/`Param`/etc. → `variable`
   - Walk the raw `ast` for `Form::Str`, `Form::Number` → `string`, `number`

3. Emit tokens as `SemanticToken` deltas (relative line/character offsets,
   as required by the LSP spec). The `syms` vec is already sorted by
   `span.start` which makes delta computation a single linear scan.

   The result needs to be stored or re-derived cheaply; since the full
   pipeline runs on every change anyway, derive it in `Workspace::update` and
   cache it alongside `analysis`.

### Code actions and formatting

**Current behaviour:** Neither is implemented.

**Proposed code actions:**

- *Quick fix for unknown identifier*: attach a `CodeAction` to each
  unknown-identifier diagnostic offering "Add `(local name nil)` at top of
  scope" or "Add `(var name nil)` here." Use `DiagnosticData` (a JSON field
  in the diagnostic) to carry the name and insertion position so the action
  handler doesn't need to re-analyze.
- *Convert `var` to `local`*: offered when the "unused var" lint fires (see
  Semantic correctness above). Rename the `var` binding and remove the `set`
  call if there is only one.

**Proposed formatter:**

Implement a pretty-printer in a new `src/fmt.rs` module that walks `Vec<AstNode>`
and emits indented text:

- Lists: `(head` on the first line, remaining items indented by 2 spaces,
  closing `)` on the last item's line.
- Special forms that take a binding vector (`let`, `fn`, `each`, …): keep the
  binding vector on the same line as the head up to a configurable column
  limit (80), break otherwise.
- Sequences and tables: inline up to the column limit, multi-line otherwise.
- Preserve comments by attaching them to the next form (lexer already emits
  line/col for every token; interleave comment tokens with AST nodes).

Wire the formatter into `textDocument/formatting` and
`textDocument/rangeFormatting`. Configuration options (indent width, column
limit) can be passed via `FormattingOptions` or a `.fennel-ls.toml` file in
the workspace root.

---

## Tree-sitter grammar (`tree-sitter-fennel/`)

A fork of [alexmozaidze/tree-sitter-fennel](https://github.com/alexmozaidze/tree-sitter-fennel)
maintained here to fix bugs. It is a **separate concern** from the LSP and
is not linked into the binary.

### Bugs fixed in this fork

**1. Unquote-splice (`,@`) was completely broken.**

The original grammar had no rule for `,@`. `@` is excluded from the symbol
regex so `,@xs` produced a parse error. Three-file fix:

- `grammar-lib/constants.js` — added `['unquote_splice', ',@']` to
  `READER_MACROS` so `nodify_reader_macros` auto-generates the
  `unquote_splice_reader_macro` rule and its external token slot.
- `src/scanner.c` — added `TK_UNQUOTE_SPLICE` to the `TokenType` enum
  (must stay ordered to match the `externals[]` array generated by
  `grammar.js`); set comma entries in `READER_MACRO_CHARS` to `0`;
  added a special `,`/`,@` branch in `scan_reader_macro`: `,@<non-ws>`
  → `TK_UNQUOTE_SPLICE`, `,<non-ws>` → `TK_UNQUOTE`,
  `,<ws|bracket|eof>` → `SCAN_FAILURE`.
- `src/parser.c` — regenerated with `tree-sitter generate`.

**2. Commas as whitespace separators produced ERROR nodes.**

`[a, b, c]` was parsed with ERROR nodes on the commas because `,` was not
in the `extras` array and had no other rule to match.

Fix: added `','` to `extras` in `grammar.js`. The external scanner takes
priority over the internal lexer, so in quasiquote positions where unquote
tokens are valid, the scanner still intercepts `,expr` as `TK_UNQUOTE`
and `,@expr` as `TK_UNQUOTE_SPLICE` before the internal lexer sees the
comma. Bare separator commas (rejected by the scanner) fall through to the
extras rule and are silently skipped.

### Regenerating the parser

After changing `grammar.js`, `grammar-lib/constants.js`, or `src/scanner.c`,
`src/parser.c` must be regenerated:

```bash
# From the project root — enter the Nix dev shell
nix develop

# Inside the shell
cd tree-sitter-fennel
npm install --ignore-scripts   # get lodash; skip native node-gyp build
tree-sitter generate

# Verify all 36 corpus tests pass
tree-sitter test
```

`grammar.js` requires `lodash` (via `npm`) but does not need the native
`tree-sitter` npm package. The Nix shell provides the `tree-sitter` CLI
directly.

### Known grammar limitations

**`[a,,,b]` — consecutive commas produce nested unquotes.**

`[a,,,b]` parses as `[a (unquote (unquote (unquote b)))]` rather than `[a b]`.
The rule `,<non-ws>` → unquote fires three times in a row because the scanner
can't distinguish quasiquote context from plain context.

**Proposed fix:** Maintain a quasiquote depth counter in the scanner's
serializable state. The tree-sitter external scanner API provides
`serialize`/`deserialize` callbacks for exactly this purpose.

```c
typedef struct { int32_t quasi_depth; } ScannerState;

void *tree_sitter_fennel_external_scanner_create(void) {
    ScannerState *s = malloc(sizeof(ScannerState));
    s->quasi_depth = 0;
    return s;
}

unsigned tree_sitter_fennel_external_scanner_serialize(void *payload, char *buf) {
    memcpy(buf, payload, sizeof(ScannerState));
    return sizeof(ScannerState);
}

void tree_sitter_fennel_external_scanner_deserialize(void *payload,
                                                      const char *buf,
                                                      unsigned len) {
    if (len == sizeof(ScannerState)) memcpy(payload, buf, len);
}
```

In `scan_reader_macro`, increment `quasi_depth` when emitting
`TK_QUASI_QUOTE` and decrement it when the unquote scan runs. The tricky
part is knowing *when* a quasiquoted form ends (there is no closing token).
One approach: track depth inside the scanner and suppress unquote/unquote-splice
production when `quasi_depth == 0`, letting the internal lexer treat `,`
as an extra (whitespace separator) instead.

The difficulty is that tree-sitter's incremental re-parsing can call
`deserialize` at any point in the file, so the depth counter must be
faithfully restored. This requires careful integration testing across edits.

**Pragmatic assessment:** Real Fennel code never writes `,,expr` outside a
quasiquote — the pattern only arises in pathological input. The stateful
scanner approach is the correct long-term solution but is high-complexity
for low-real-world-benefit. It is a good candidate for implementation only
after the LSP-side cross-file analysis work is complete.

---

## Test coverage

```
src/lexer.rs    — 71 unit tests (delimiters, atoms, numbers, strings, reader
                  macros, whitespace, spans, comma semantics, corpus-driven
                  edge cases)

src/parser.rs   — 80 unit tests (atoms, lists, sequences, tables, reader macros,
                  real Fennel patterns, spans, error recovery, corpus-driven
                  cases: hashfn, method calls, destructuring, match guards,
                  icollect, accumulate, import-macros)

src/analyzer.rs — 123 unit tests (scope resolution, all form handlers, lint
                  rules: arity, unused local, unused param, varargs edge cases)

src/docs.rs     — 22 unit tests (Platform variants, BuiltinSet, multisym
                  lookup, custom extras)

src/workspace.rs — 9 unit tests (default platform, configure_platform,
                   LuaJIT extras, custom extras, with_builtins)

src/server.rs   — 44 unit tests (format_definition, def_kind_to_symbol_kind,
                  known_global, find_var_keyword_before, require_module_at,
                  apply_incremental_changes, build_semantic_tokens,
                  platform_from_str)

src/text.rs     — 15 unit tests

src/config.rs   — 5 unit tests (TOML parsing, defaults, error handling)

tree-sitter-fennel/test/corpus/
  edge-cases.txt  — comma-as-separator, unquote_splice in sequence, multiple
                    splices, empty forms, trailing comma, hashfn on multi-symbol,
                    unfinished tables, colon-string + reader macros, shebang
  literals.txt    — numbers, strings, colon strings, symbols, symbol options,
                    multi-symbols, booleans/nil, tables, sequences, comments,
                    unquote_splice reader macro, all other reader macros
  forms.txt       — quote & unquote, bindings, let, fn, hashfn, match/case,
                    case-try, match-try, case guard, each, if, import-macros, macro
  statements.txt  — shebang, function call, method call
```

Total: 36 tree-sitter corpus tests (all passing), 362 Rust unit tests.
