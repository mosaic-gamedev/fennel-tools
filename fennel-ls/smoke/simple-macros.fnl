;; simple-macros.fnl — macro module used by the smoke tests.
;; Consumed via (import-macros {: defsimple} :simple-macros) etc.

(fn defsimple [name value]
  `(local ,name ,value))

;; Stand-in for the defnode macro (real version lives in lua-gdextension).
;; For smoke tests: collects (fn ...) bodies and emits them as local functions.
(fn defnode [name ...]
  (local result `(do))
  (local node-local `(local ,name {}))
  (table.insert result node-local)
  (each [_ form (ipairs [...])]
    (when (= (tostring (. form 1)) :fn)
      (local method-name (. form 2))
      (local params (. form 3))
      (local fn-form `(fn ,method-name ,params))
      (for [i 4 (length form)]
        (table.insert fn-form (. form i)))
      (table.insert result fn-form)))
  (table.insert result name)
  result)

{:defsimple defsimple :defnode defnode}
