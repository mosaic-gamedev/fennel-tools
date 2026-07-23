;; simple-macros.fnl — macro module used by the smoke tests.
;; Consumed via (import-macros {: defsimple} :simple-macros).

(fn defsimple [name value]
  `(local ,name ,value))

{:defsimple defsimple}
