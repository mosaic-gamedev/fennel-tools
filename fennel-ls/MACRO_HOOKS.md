# Macro Hooks

Fennel macros are opaque to the language server — it cannot see inside them to know what names they bind, what sub-forms are functions, or what scopes they open. Macro hooks let you teach the LSP about a macro's semantics so completions, definitions, and diagnostics work correctly at call sites.

---

## Configuration

Hooks are registered in `.lsp.fnl` under the `:macro-hooks` key. Each hook is a Fennel function that receives the call node and returns a list of instructions.

```fennel
;; .lsp.fnl
{:macro-hooks
  {:my-macro (fn [call] [...instructions...])}}
```

---

## The Call Node

The hook receives one argument: the macro call as a tree of nodes. Each node has:

| Field        | Type            | Description                          |
|--------------|-----------------|--------------------------------------|
| `kind`       | string          | `"list"`, `"symbol"`, `"string"`, `"number"`, `"keyword"`, `"bool"`, `"vararg"` |
| `value`      | string or nil   | Token text for leaf nodes            |
| `children`   | table or nil    | Child nodes for list nodes           |
| `span`       | `{start end}`   | Byte offsets in the source file      |

For a call `(defnode Foo (fn _ready [self] ...))`:
- `call.children[1]` → symbol `defnode`
- `call.children[2]` → symbol `Foo`
- `call.children[3]` → list `(fn _ready [self] ...)`

---

## Instructions

A hook returns a list (table) of instruction tables. Return an empty list `[]` to suppress the default macro-body analysis without doing anything.

### `:bind` — introduce a name into scope

```fennel
{:type :bind :name "Foo" :span call.children[2].span}
```

Creates a local definition at the given span. The name becomes visible to completions, `goto-definition`, and references.

### `:analyze` — analyze a sub-form normally

```fennel
{:type :analyze :index 3}
```

Runs the standard analyzer on `call.children[index]`. Use this for sub-forms that are plain expressions — the analyzer will resolve identifiers and collect definitions from them.

### `:analyze-fn` — analyze a sub-form as a function body

```fennel
{:type :analyze-fn :index 3}
```

Like `:analyze`, but treats the sub-form as a `fn` form. Parameters are bound into scope, the body is analyzed, and any nested `fn` definitions are collected as real symbols. Use this when a sub-form has the shape `(fn name [params] body...)`.

### `:scope-open` / `:scope-close` — bracket a new scope

```fennel
{:type :scope-open}
{:type :scope-close}
```

Any `:bind` or `:analyze` instructions between these two see each other but are hidden from the outer scope. Useful when a macro creates an isolated block.

---

## Example — `defnode`

`defnode` binds a class name and treats each `(fn ...)` sub-form as a real function:

```fennel
;; in hooks-macro.fnl
(import-macros {: defnode} :simple-macros)

(defnode FennelNode3D
  (fn _ready [self] ...)
  (fn _process [self delta] ...))
```

Hook:

```fennel
{:macro-hooks
  {:defnode
    (fn [call]
      (let [result []
            name-node (. call.children 2)]
        ;; Bind the class name
        (table.insert result {:type :bind
                              :name name-node.value
                              :span name-node.span})
        ;; Analyze each (fn ...) sub-form as a real function
        (for [i 3 (length call.children)]
          (let [form (. call.children i)
                head (and form.children (. form.children 1))]
            (when (and head (= head.value :fn))
              (table.insert result {:type :analyze-fn :index i}))))
        result))}}
```

After this hook runs:
- `FennelNode3D` appears in `workspace/symbol` and goto-definition works on it
- `_ready` and `_process` appear as function symbols with correct parameter lists

---

## Module-qualified keys

If two macro modules export the same name, use a nested table keyed by module path to avoid collisions. Module-specific hooks take priority; the flat key acts as a fallback for any module.

```fennel
{:macro-hooks
  {;; applies to defnode from any module
   :defnode flat-hook
   ;; overrides the flat hook for defnode from this specific module
   "my-project.macros" {:defnode specific-hook}}}
```

The module key matches the string passed to `import-macros`:

```fennel
(import-macros {: defnode} :my-project.macros)
;;                          ^^^^^^^^^^^^^^^^^ this string
```

---

## Warn on unhooked macros

To get a hint-level diagnostic on every macro call that has no registered hook:

```fennel
;; .lsp.fnl
{:warn-unhooked-macros true
 :macro-hooks {...}}
```

This is off by default. When enabled, each unhooked macro call produces a diagnostic with severity **Hint**, so it is visible but non-intrusive.
