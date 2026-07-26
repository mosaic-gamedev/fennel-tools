# fennel-ls LSP Feature Coverage

Status legend: **✅ done** · **❌ not implemented** · **N/A not applicable**

---

## Lifecycle

| Feature | Status | Notes |
|---------|--------|-------|
| `initialize` | ✅ | Returns full capability set |
| `initialized` | ✅ | Registers `*.fnl` file watcher |
| `shutdown` | ✅ | |
| `exit` | ✅ | Handled by tower-lsp |
| `$/cancelRequest` | ✅ | Handled by tower-lsp |
| `$/progress` | ❌ | Analysis completes in < 1 ms; there is no long-running operation to report progress for |
| `window/showMessage` | ❌ | This is a server-to-client send (not a handler); we use `logMessage` instead — modal popups are disruptive UX |

---

## Text Document Synchronization

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/didOpen` | ✅ | Parses, analyzes, publishes diagnostics |
| `textDocument/didChange` | ✅ | Incremental and full-text sync |
| `textDocument/willSave` | ✅ | Acknowledged notification |
| `textDocument/willSaveWaitUntil` | ✅ | Returns formatting edits before save |
| `textDocument/didSave` | ✅ | Re-analyzes on save |
| `textDocument/didClose` | ✅ | Clears diagnostics, removes from workspace |

---

## Diagnostics

| Feature | Status | Notes |
|---------|--------|-------|
| Push diagnostics (`publishDiagnostics`) | ✅ | Sent after every open/change/save |
| Parse errors | ✅ | Syntax errors with spans |
| Undefined identifier warnings | ✅ | Respects `known_globals` from config |
| Unused local warnings | ✅ | Skips `_`-prefixed names |
| Unused parameter warnings | ✅ | Skips `_`-prefixed and destructured params |
| Immutability violation (`set` on `local`) | ✅ | Suggests `var` |
| Never-mutated `var` | ✅ | Suggests `local` |
| Shadow warnings | ✅ | Warns on same-scope redefinition |
| Arity mismatch | ✅ | Checks call sites against param counts |
| `source` field | ✅ | All diagnostics tagged `"fennel-ls"` |
| `code` field | ✅ | String codes: `shadow`, `unused-local`, `unused-param`, `never-mutated`, `immutable`, `arity`, `unknown` |
| `relatedInformation` | ✅ | Shadow warnings link to original definition |
| Diagnostic tags (`UNNECESSARY`) | ✅ | Unused locals/params shown dimmed in editors |
| Pull diagnostics (`textDocument/diagnostic`) | ❌ | The push model covers all real-world use cases; pull (LSP 3.17) would require duplicating the diagnostic pipeline into a separate request/response path for no user-visible benefit |
| Workspace-level pull diagnostics | ❌ | Same reason as above; also requires `workspace/diagnostic` which is LSP 3.17 and not yet in our lsp-types version |

---

## Hover

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/hover` | ✅ | |
| Hover on local/fn definitions | ✅ | Shows signature and inline docstring |
| Hover on builtin specials | ✅ | Fennel built-in docs embedded |
| Hover on Lua builtins | ✅ | Platform-specific (LuaJIT / Lua 5.x) |
| Hover on globals from config | ✅ | Reads `global_docs` from `.lsp.fnl` |
| Hover on cross-file symbols | ✅ | Follows require chain |
| Markdown content format | ✅ | Code fences + docstring prose |

---

## Navigation

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/definition` | ✅ | Same-file and cross-file via `require` |
| `textDocument/declaration` | ✅ | Alias to definition (no separate declaration in Fennel) |
| `textDocument/references` | ✅ | Same-file and cross-file |
| `textDocument/documentHighlight` | ✅ | Highlights all uses of symbol under cursor |
| `textDocument/typeDefinition` | N/A | Fennel is dynamically typed; there are no static types to navigate to |
| `textDocument/implementation` | N/A | No interface/implementation split in Fennel |

---

## Completion

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/completion` | ✅ | |
| Local bindings | ✅ | All in-scope locals |
| Fennel special forms & macros | ✅ | |
| Lua / Fennel builtins | ✅ | Platform-aware |
| Module member completion (`mod.`) | ✅ | Follows `require` to imported file's exports |
| Method call completion (`obj:`) | ✅ | |
| Globals from config | ✅ | `known_globals` + `global_docs` keys |
| Trigger characters | ✅ | `(`, `[`, `{`, `.`, `:` |
| `completionItem/resolve` | ❌ | We already send `label`, `kind`, `detail`, `documentation`, and `insertText` in the initial response. Resolve exists to defer expensive fields until the user focuses an item — there is nothing left to defer |

---

## Signature Help

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/signatureHelp` | ✅ | |
| Parameter list display | ✅ | |
| Active parameter highlighting | ✅ | Tracks cursor position within arg list |
| Trigger characters | ✅ | `(` and space |
| Variadic functions | ✅ | Rest args shown as `& rest` |

---

## Symbols

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/documentSymbol` | ✅ | Hierarchical (`DocumentSymbol` tree) |
| `workspace/symbol` | ✅ | Searches all open files |
| `workspaceSymbol/resolve` | ❌ | We already return `name`, `kind`, `location`, and `containerName` in the initial list. Nothing to add in a resolve step |
| Functions (`fn`) | ✅ | |
| Locals (`local`, `var`) | ✅ | |
| Nested definitions | ✅ | Inner `fn` shown as child of enclosing form |

---

## Code Actions

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/codeAction` | ✅ | |
| Quickfix: `var → local` | ✅ | Triggered by "never mutated" diagnostic |
| Quickfix: Remove unused local | ✅ | Removes whole `(local ...)` form |
| Refactor: `local → var` | ✅ | Offered on any `local` binding name |
| Refactor: Wrap in `(do ...)` | ✅ | Offered for any non-empty selection |
| `codeAction/resolve` | ❌ | We already compute and return the full `WorkspaceEdit` in the initial response. Resolve exists to defer edit computation until the user actually applies an action — our edits are cheap to compute upfront |
| `textDocument/codeLens` | ✅ | Shows reference count above each `fn` definition |
| `codeLens/resolve` | ❌ | We already include the `command.title` (reference count) in the initial lens. Nothing to resolve |

---

## Rename

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/prepareRename` | ✅ | Returns range + placeholder; rejects non-renameable positions |
| `textDocument/rename` | ✅ | Cross-file; updates all references including module-qualified names |

---

## Formatting

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/formatting` | ✅ | Full-file; powered by `fennel-format` |
| `textDocument/rangeFormatting` | ✅ | Expands selection to complete top-level forms |
| `textDocument/onTypeFormatting` | ✅ | Auto-indents on Enter via forward-scan paren tracker |

---

## Folding & Selection

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/foldingRange` | ✅ | AST-based; folds `fn`, `let`, `do`, etc. |
| `textDocument/selectionRange` | ✅ | Expands selection up the AST |
| `textDocument/linkedEditingRange` | N/A | Designed for paired HTML/XML tags; no equivalent construct in Fennel |

---

## Call Hierarchy

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/prepareCallHierarchy` | ✅ | Resolves cursor to named `fn` definition |
| `callHierarchy/incomingCalls` | ✅ | Cross-file; groups multiple call sites per caller |
| `callHierarchy/outgoingCalls` | ✅ | Walks fn body AST; deduplicates by callee |

---

## Type Hierarchy

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/prepareTypeHierarchy` | N/A | Fennel is dynamically typed; there are no static types to build a hierarchy from |
| `typeHierarchy/supertypes` | N/A | |
| `typeHierarchy/subtypes` | N/A | |

---

## Semantic Tokens

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/semanticTokens/full` | ✅ | Functions, parameters, variables, macros; definition + readonly modifiers |
| `textDocument/semanticTokens/full/delta` | ✅ | Single-edit delta computed from a per-file flat-u32 cache |
| `textDocument/semanticTokens/range` | ✅ | Decodes the full token stream and filters to the requested line range |

---

## Inlay Hints

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/inlayHint` | ✅ | Parameter name hints at call sites |
| `inlayHint/resolve` | ❌ | We already return the full label and position in the initial response. Nothing to resolve |

---

## Document Links

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/documentLink` | ✅ | `(require :mod)` → clickable link to resolved file |
| `documentLink/resolve` | ❌ | We already include `target` (file URI) and `tooltip` (path) in the initial link. Nothing to resolve |

---

## Miscellaneous Text Document

| Feature | Status | Notes |
|---------|--------|-------|
| `textDocument/documentColor` | N/A | No color literals in Fennel |
| `textDocument/colorPresentation` | N/A | |
| `textDocument/inlineValue` | ✅ | Returns `InlineValueVariableLookup` for every binding in scope at the stopped location; DAP resolves the actual values |
| `textDocument/moniker` | ❌ | Used by code-intelligence indexers (LSIF, SCIP, Sourcegraph) to assign globally unique names to symbols. Not an editing feature; only needed if publishing an index |

---

## Workspace Management

| Feature | Status | Notes |
|---------|--------|-------|
| `workspace/didChangeConfiguration` | ✅ | Re-reads `.lsp.fnl`; re-publishes diagnostics |
| `workspace/symbol` | ✅ | |
| `workspace/didChangeWatchedFiles` | ✅ | Reloads required-but-not-open files changed on disk |
| `workspace/didCreateFiles` | ✅ | Invalidates require cache; indexes new file |
| `workspace/didRenameFiles` | ✅ | Moves cache entry; reloads file at new path |
| `workspace/didDeleteFiles` | ✅ | Removes from workspace; invalidates cache |
| `workspace/willRenameFiles` | ✅ | Returns edits updating `(require :old)` → `(require :new)` in all open files |
| `workspace/willCreateFiles` | ❌ | The server has no edits to propose before a file is created — there is no existing content to rewrite |
| `workspace/willDeleteFiles` | ❌ | Could theoretically offer to remove `(require :mod)` from files that depend on the deleted file, but this is destructive and hard to do correctly (the binding may be used in complex ways). Covered well enough by existing diagnostics that will flag the broken require after deletion |
| `workspace/executeCommand` | ❌ | No custom commands are currently defined. Would need a concrete use case (e.g. `fennel-ls.reloadConfig`) before wiring this up |
| `workspace/diagnostic` (pull) | ❌ | LSP 3.17 pull model. Push (`publishDiagnostics`) covers all practical use cases and is simpler to reason about. Adding pull would require maintaining a separate diagnostic-state machine in parallel with the push pipeline |

---

## Summary

| Category | ✅ Done | ❌ Not Implemented | N/A |
|----------|---------|-------------------|-----|
| Lifecycle | 5 | 2 | 0 |
| Text Sync | 6 | 0 | 0 |
| Diagnostics | 13 | 2 | 0 |
| Hover | 7 | 0 | 0 |
| Navigation | 4 | 0 | 2 |
| Completion | 8 | 1 | 0 |
| Signature Help | 5 | 0 | 0 |
| Symbols | 6 | 1 | 0 |
| Code Actions | 6 | 2 | 0 |
| Rename | 2 | 0 | 0 |
| Formatting | 3 | 0 | 0 |
| Folding & Selection | 2 | 0 | 1 |
| Call Hierarchy | 3 | 0 | 0 |
| Type Hierarchy | 0 | 0 | 3 |
| Semantic Tokens | 3 | 0 | 0 |
| Inlay Hints | 1 | 1 | 0 |
| Document Links | 1 | 1 | 0 |
| Misc Text Document | 1 | 1 | 2 |
| Workspace Management | 7 | 4 | 0 |
| **Total** | **83** | **15** | **8** |
