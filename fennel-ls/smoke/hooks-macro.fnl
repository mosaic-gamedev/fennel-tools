;; hooks-macro.fnl — smoke test for macro hooks.
;;
;; The run_smoke.py test opens this file with a .lsp.fnl that registers a hook
;; for the `defnode` macro (imported from simple-macros.fnl for the smoke test).
;; After the hook pass, FennelNode3D should be a real definition (not in_macro),
;; and the _ready method should be analyzable as a function.
;;
;; For the smoke test we use a simple stand-in macro that mimics defnode's
;; structure so we don't need the full Godot extension available.

(import-macros {: defnode} :simple-macros)

(defnode FennelNode3D
  (fn _ready [self]
    (print "ready"))
  (fn _process [self delta]
    (print delta)))
