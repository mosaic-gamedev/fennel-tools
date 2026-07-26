;; macro-user.fnl — smoke test for macro expansion.
;;
;; After did_open the expander runs fennel.compileString on this source,
;; discovers 'defsimple' (from scope.macros) and 'answer' (from
;; scope.unmanglings), and stores both in macro_globals. Neither should
;; trigger "unknown identifier" diagnostics.

(import-macros {: defsimple} :simple-macros)
(defsimple answer 42)
(print answer)
