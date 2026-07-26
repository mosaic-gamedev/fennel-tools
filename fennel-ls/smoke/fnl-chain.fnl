;; fnl-chain.fnl
;; Middle link in the nested-require chain: this Fennel file requires a Lua module.
;;
;; Smoke test goals (workspace root = smoke/):
;;
;;   NO UNKNOWN-IDENTIFIER WARNINGS
;;     - chain.double and chain.square must not produce warnings
;;
;;   GOTO-DEF (Fennel → Lua, nested hop)
;;     - cursor on chain.double → lua-chain.lua, line containing "double"
;;     - cursor on chain.square → lua-chain.lua, line containing "square"

(local chain (require :lua-chain))

(fn compute [x]
  (+ (chain.double x) (chain.square x)))
