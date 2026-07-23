;; code-actions.fnl — dedicated test file for code action smoke tests

;; This binding is intentionally unused to trigger "remove unused local" quickfix.
(local test-unused 42)

;; This binding is intentionally left as `local` to trigger "local→var" refactor.
(local test-mutable 1)

;; Suppress diagnostics about the other unused variable so warning count is predictable.
(local _keep test-mutable)
