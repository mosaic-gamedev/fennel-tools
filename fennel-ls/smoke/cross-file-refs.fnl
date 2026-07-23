;; cross-file-refs.fnl — consumer file for workspace-symbol / cross-file-refs smoke tests
;;
;; Smoke test goals (workspace root = smoke/, both files open):
;;
;;   WORKSPACE SYMBOLS
;;     - query "greet"   → finds geometry.vec2 (no), finds greet/bye from utils
;;       (this file is not the one exporting — but geometry.* defs appear too)
;;     - query "vec2"    → finds vec2, vec2-add, vec2-scale, vec2-dot, vec2-length
;;     - query ""        → returns everything across all open files
;;
;;   FIND REFERENCES (cross-file)
;;     - cursor on `greet` definition in utils.fnl
;;       → finds (utils.greet) and (utils.greet 1) references in this file
;;     - cursor on `utils.greet` in this file
;;       → resolves back to the same two refs in this file
;;     - cursor on `bye` definition in utils.fnl
;;       → finds (utils.bye) reference in this file
;;
;;   RENAME (cross-file)
;;     - rename `greet` in utils.fnl to `hello`
;;       → edit in utils.fnl: `greet` → `hello`
;;       → edit in this file: `utils.greet` → `utils.hello` (two sites)
;;     - rename `bye` in utils.fnl to `farewell`
;;       → edit in utils.fnl + one edit in this file

(local utils (require :utils))

(utils.greet "world")
(utils.greet 1)
(utils.bye)
