;; .lsp.fnl — fennel-ls configuration for the smoke test workspace.
;;
;; Registers a hook for the `defnode` macro (as exported by simple-macros.fnl)
;; so the smoke test can verify end-to-end hook execution.

{:macro-hooks
  {:simple-macros
    {:defnode
      (fn [call]
        ;; call.children[1] = defnode (head sym)
        ;; call.children[2] = class name sym (e.g. FennelNode3D)
        ;; call.children[3+] = DSL sub-forms
        (let [result []
              name-node (. call.children 2)]
          ;; Bind the class name as a local definition
          (table.insert result
            {:type :bind
             :name name-node.value
             :span name-node.span})
          ;; For each (fn name [params] body...) sub-form, analyze as fn
          (for [i 3 (length call.children)]
            (let [form (. call.children i)
                  head (and form.children (. form.children 1))]
              (when (and head (= head.value :fn))
                (table.insert result {:type :analyze-fn :index i}))))
          result))}}}
