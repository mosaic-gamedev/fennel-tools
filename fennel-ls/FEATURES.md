# LSP Feature Status

---

## Implemented

### Text synchronisation
Incremental sync (`TextDocumentSyncKind::INCREMENTAL`). The client sends only
the changed ranges; each change event is applied in sequence to the in-memory
text, then the full pipeline re-runs. The lexer → parser → analyzer pipeline
re-runs on each `didOpen`, `didChange`, and `didSave`.

### Diagnostics
Pushed to the client after every sync event. Sources:

| Kind | Severity | Examples |
|---|---|---|
| Parse errors | ERROR | Unclosed delimiter, unexpected token |
| Semantic warnings | WARNING | `set` on an immutable `local`, `var` that is never mutated, same-scope shadowing |
| Unknown identifiers | WARNING | Symbol not in scope, not a builtin, not in `known_global` fallback list |
| Arity errors | WARNING | Calling a function with too few or too many arguments (suppressed for variadic functions) |
| Unused locals | WARNING | `local`/`let` binding never referenced (suppress with `_`-prefixed name) |
| Unused params | WARNING | Function parameter never referenced (suppress with `_`-prefixed name) |

Multisym names (`io.open`, `obj:method`) are suppressed if the root (`io`,
`obj`) is known.

### Hover
- **User-defined names:** renders `(kind name [params])` in a fenced Fennel
  code block, followed by the inline docstring if the function body begins
  with a string literal.
- **Built-in names:** renders the signature and description from `docs.rs`.
  Multisym lookup (`string.format`) resolves to the root entry (`string`).
- **Cross-file module members:** `(local utils (require :utils))` followed by
  hovering on `utils.greet` shows `greet`'s signature and docstring from
  `utils.fnl`.
- **Custom global docs:** signatures and docs from `global_docs` entries in
  `.fennel-ls.toml` (see Configuration).

### Go to definition
Jumps to the binding site of the symbol under the cursor.
- **Same-file:** resolves to the definition in the current file.
- **Module members:** `utils.greet` → jumps to `greet`'s definition in `utils.fnl`.
- **Require strings:** cursor on `:utils` inside `(require :utils)` → opens `utils.fnl`.

### Find references
Returns all use-sites of whichever definition the cursor is on (or the
definition a reference points to). Single-file only. Includes the definition
itself.

### Document highlight
Same logic as find-references. Returns `WRITE` highlight kind for the
definition and `READ` for uses.

### Document symbols
Lists every definition in the file (`local`, `var`, `global`, `fn`, `macro`,
`param`, `loop-var`, destructured). Maps `DefKind` to LSP `SymbolKind`.

### Rename
Renames all occurrences of a symbol within the current file. Single-file only.


### Semantic tokens (`textDocument/semanticTokens/full`)
Per-token classification for richer editor highlighting beyond what a static
grammar provides. Token types: `function`, `parameter`, `variable`, `macro`.
Modifiers: `definition` (binding site) and `readonly` (immutable bindings).
Built-in references and unresolved symbols are omitted (grammar handles base
coloring). Delta-encoded as required by the LSP spec.

### Configuration file (`.fennel-ls.toml`)
An optional file in the workspace root to configure the server without
recompiling. Supported fields:

```toml
platform = "lua54"   # lua51 | lua52 | lua53 | lua54 (default) | luajit | luau
known_globals = ["love", "vim", "hs"]
```

`platform` selects the active `BuiltinSet`, controlling which globals are
known. `known_globals` suppresses unknown-identifier warnings for names that
are not in any `BuiltinSet` (e.g. game framework or editor globals).

### Completion
Triggered on `(`, `[`, `{`, `.`, `:`.

- Scope-local definitions, respecting lexical scope at the cursor position,
  with param and docstring info where available.
- All built-ins from the active `BuiltinSet`.
- **Cross-file module members:** typing `utils.` after `(local utils (require :utils))`
  offers all top-level exports from `utils.fnl` with labels like `utils.greet`.
- Custom global docs from `.fennel-ls.toml` filtered to the current multisym prefix.
- Deduplicated by name (innermost binding wins). Sorted alphabetically.
- `CompletionItemKind` follows `DefKind` (Fn → FUNCTION, Macro → KEYWORD,
  LoopVar/Param → VARIABLE, etc.) and `BuiltinKind` (Function → FUNCTION,
  SpecialForm/Macro → KEYWORD, Value → MODULE).

### Folding ranges (`textDocument/foldingRange`)
Multi-line lists, sequences, and tables each produce a folding region. Nested
structures each fold independently.

### Signature help (`textDocument/signatureHelp`)
Shows the parameter list of the function being called as the user types
arguments. Triggered on `(` and space. The active parameter is highlighted as
the cursor moves through the argument positions. Only fires when the head
symbol resolves to a user-defined `fn` with a known parameter list.

### Inlay hints (`textDocument/inlayHint`)
Parameter-name hints at call sites — shows which parameter each positional
argument maps to (e.g. `a:` before the first argument). Suppressed for
`_`-prefixed parameters (conventional discard) and rest parameters (`& rest`).

### Code actions
- **`var → local` quickfix:** when a `var` binding was never mutated, rewrite the `var` keyword to `local`.
- **Unknown-identifier stub:** when the cursor is on an unknown-identifier warning, insert `(local name nil)` on the line above as a starting point.

---

## Should be implemented

### Workspace symbols (`workspace/symbol`)
Search all open (or project-wide) files for definitions matching a query
string. Straightforward extension of the per-file document-symbol handler;
cross-file require resolution means the dependency graph already exists.

### Formatting (`textDocument/formatting`)
A pretty-printer that walks `Vec<AstNode>` and emits canonically indented
Fennel. Implementation in a new `src/fmt.rs`. The lexer already records
line/col for every token; comment tokens can be interleaved with AST nodes to
preserve them.

### Macro expansion at call sites
Macros introduce names into the caller's scope that the static analyzer
cannot see. The right long-term fix is to spawn `fennel --expand` on a call
site and parse the result. A feature flag can gate this on the presence of
Fennel on `PATH`.

### Cross-file references and rename
Find-references and rename currently operate within the current file only.
Extending them cross-file requires scanning all files that transitively
require the file containing the renamed definition — the require_cache already
has the per-file module graph needed to compute this set.

---

## Not worth implementing

### Type checking / type inference
Fennel and Lua have no static type system. Inferring types would require
whole-program analysis, an understanding of Lua metatables, and integration
with LuaJIT's FFI — an enormous, open-ended effort. The payoff is low because
users of a dynamically typed language don't expect type errors from their
editor.

### Call hierarchy (`callHierarchy/incomingCalls`, `outgoingCalls`)
Fennel is functional: functions are first-class values, higher-order functions
are idiomatic, and tables-of-functions are common. Static call graphs are
therefore misleading — they miss dynamic dispatch entirely and surface false
call sites for things like `(each [_ f (ipairs handlers)] (f))`. The feature
would have low signal and high noise.

### Inlay hints for types
No type system to infer from. Parameter-name hints at call sites are
implemented (see above); type-level hints have nothing to anchor to.

### `textDocument/implementation` and `textDocument/typeDefinition`
These navigate from a type or interface declaration to its implementation, or
from a usage to the type definition. Lua has no interfaces or abstract types.
These requests have no sensible semantic in Fennel.

### Linked editing ranges
Allows renaming paired tags simultaneously (HTML `<div>…</div>` use case).
Lisp has no paired open/close constructs beyond brackets, and bracket matching
is already handled by the editor's built-in parser or tree-sitter. There is
nothing to link.

### Selection range (`textDocument/selectionRange`)
Structural selection (expand selection to enclosing s-expression, then to
the enclosing form, etc.) is useful in Lisps, but editors already provide
this through tree-sitter or their own bracket-aware selection logic. The LSP
version would duplicate what the editor already does better.

### Moniker (`textDocument/moniker`)
Cross-repository symbol resolution for package registries. Fennel has no
package registry in the npm/crates.io sense. Not applicable.

### Code lens
Code lenses show actionable information inline in the editor (e.g. "Run test",
"N references"). Every useful lens for Fennel would require executing code
(running tests, evaluating expressions) or cross-file reference counts that
are expensive to maintain. The static analyzer alone cannot produce lens
content worth showing.
