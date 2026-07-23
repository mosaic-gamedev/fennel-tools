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
definition a reference points to). Includes the definition itself.
- **Same-file:** all references in the current file.
- **Cross-file:** if the cursor is on a top-level definition (or a multisym
  reference to one), also returns every `binding.def_name` site in all open
  files that import the definition's file.

### Document highlight
Same logic as find-references. Returns `WRITE` highlight kind for the
definition and `READ` for uses.

### Document symbols
Lists every definition in the file (`local`, `var`, `global`, `fn`, `macro`,
`param`, `loop-var`, destructured). Maps `DefKind` to LSP `SymbolKind`.

### Workspace symbols (`workspace/symbol`)
Searches all open files and the require-cache for definitions matching a query
string (case-insensitive substring match). Returns the definition's name, kind,
and location in its source file. Powered by `all_defs()` in `workspace.rs`.

### Rename
Renames all occurrences of a symbol.
- **Same-file:** all occurrences of the binding in the current file.
- **Cross-file:** if renaming a top-level definition (or a multisym that
  resolves to one), also rewrites every `binding.old_name` to
  `binding.new_name` across all open files that import the definition's file,
  preserving the `.` or `:` separator.


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

### Formatting (`textDocument/formatting`)
Pretty-printer, enabled by default. Disable by passing `--no-formatting` when
starting the server (e.g. `fennel-ls --no-formatting`).

When enabled:
- Short forms (≤ 80 characters flat) are kept on one line.
- **Body forms** (`fn`, `let`, `if`, `when`, `match`, `each`, `for`, etc.) pack
  atom/sequence arguments onto the head line; the first compound (`List`/`Table`)
  child breaks to a new indented line (2-space indent, same as Clojure convention).
- **Regular calls** pack arguments greedily until the column limit is exceeded,
  then break.
- **Comments** are preserved in-place and always placed on their own line.
- Blank line between each pair of top-level forms; no blank line between a
  leading comment and the form it annotates.
- Single trailing newline.
- Returns no edits (no-op) if the source has parse errors, so broken files are
  never mangled.
- Atoms are emitted verbatim from the source text (preserves `0xff`, string
  escapes, and other exact spellings).

## Should be implemented

### Macro expansion at call sites
Macros introduce names into the caller's scope that the static analyzer
cannot see. The right long-term fix is to spawn `fennel --expand` on a call
site and parse the result. A feature flag can gate this on the presence of
Fennel on `PATH`.

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
