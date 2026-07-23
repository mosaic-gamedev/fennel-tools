# fennel-ls TODO

Missing LSP features, roughly in priority order.

---

## High priority — all done

All three high-priority items are implemented:
- `prepareRename` — confirms cursor is on a renameable symbol, returns range with placeholder
- `selectionRange` — walks AST upward from cursor, returns chain of enclosing spans
- Hierarchical `documentSymbol` — returns `DocumentSymbol` tree (nested), not deprecated flat list

---

## Medium priority — all done

All four medium-priority items are now implemented:
- `workspace/didChangeConfiguration` — re-reads `.fennel-ls.toml` on config change
- `textDocument/rangeFormatting` — formats the selected top-level forms only
- Call hierarchy (`prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`)
- Code actions: remove unused local, `local → var`, wrap in `(do ...)`

---

## Lower priority — all done

- Diagnostic `relatedInformation` — shadow warnings now point to the original definition
- Diagnostic `code` + `tags` — string error codes and `UNNECESSARY` tags on unused warnings
- `textDocument/declaration` — alias to definition (no separate declaration in Fennel)
- `textDocument/willSave` / `willSaveWaitUntil` — acknowledged / returns format edits
- `textDocument/documentLink` — `(require :mod)` → clickable link to resolved file
- `textDocument/codeLens` — reference count above each `fn` definition
- `textDocument/semanticTokens/full/delta` — single-edit delta with per-file cache
- `workspace/didChangeWatchedFiles` — reloads required-but-not-open `.fnl` files changed on disk
- `workspace/willRenameFiles` — returns edits updating require strings in all open files
- `workspace/did{Create,Rename,Delete}Files` — cache invalidation and re-analysis
- `semanticTokens/range` — filters full token stream to requested line range by decoding delta-encoded counters
- `textDocument/onTypeFormatting` — auto-indent on Enter via forward-scanning paren tracker
- `textDocument/inlineValue` — returns in-scope variable lookups at the DAP stopped location; values resolved by the debug adapter

## Remaining gaps (intentionally deferred)

- `completionItem/resolve`, `codeAction/resolve`, `workspaceSymbol/resolve`, `inlayHint/resolve`, `codeLens/resolve` — all data already sent upfront; resolve step adds nothing
- `workspace/willCreateFiles`, `workspace/willDeleteFiles` — nothing useful to rewrite before those events
- `workspace/executeCommand` — no custom commands defined
- `workspace/diagnostic` (pull model) — push is sufficient
- `textDocument/moniker` — only needed for LSIF/SCIP indexers, not an editing feature
- `$/progress`, `window/showMessage` — analysis is instant; no long-running ops to report
