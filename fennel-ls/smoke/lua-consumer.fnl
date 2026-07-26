;; lua-consumer.fnl
;; Requires a Lua module to test cross-language analysis.
;;
;; Smoke test goals (workspace root = smoke/):
;;
;;   NO UNKNOWN-IDENTIFIER WARNINGS
;;     - api.add, api.greet, api.answer must not produce warnings
;;
;;   GOTO-DEF (cross-language)
;;     - cursor on api.add   → lua-api.lua, LSP line 5 (function add)
;;     - cursor on api.greet → lua-api.lua, LSP line 9 (function greet)
;;
;;   COMPLETION
;;     - after "api." → suggests add, greet, answer

(local api (require :lua-api))

(fn use-api [x y]
  (api.add x y))

(fn greet-user [name]
  (api.greet name))

(fn get-answer []
  (api.answer))
