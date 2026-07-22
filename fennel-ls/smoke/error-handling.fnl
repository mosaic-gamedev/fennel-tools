;; error-handling.fnl — case-try, match-try, pcall, xpcall, error recovery
;;
;; Smoke test goals:
;;   - catch arm bindings in scope (was a bug: both bare and (catch ...) syntax)
;;   - multi-arm catch with different pattern types
;;   - chained case-try where each binding is visible in the next expression
;;   - pcall multi-value destructuring
;;   - xpcall with message handler
;;   - nested case-try
;;
;; Expected: zero warnings

;; ── case-try: bare catch syntax (old) ────────────────────────────────────────

(fn try-open-bare [path]
  (case-try
    (io.open path :r) f
    (f:read :*a)       content
    content
    catch
    msg (values nil msg)))

;; ── case-try: parenthesised catch syntax (new) ───────────────────────────────

(fn try-open [path]
  (case-try
    (io.open path :r) f
    (f:read :*a)       content
    content
    (catch
      msg (values nil msg))))

(fn try-parse-number [s]
  (case-try
    (tonumber s)  n
    (> n 0)       positive
    n
    (catch
      :nil-result (values nil "not a number")
      msg         (values nil (tostring msg)))))

;; ── case-try: multi-arm catch with pattern literals ──────────────────────────

(fn categorise-error [thunk]
  (case-try
    (thunk) result
    result
    (catch
      :not-found   :missing
      :permission  :denied
      err          (.. :unknown/ (tostring err)))))

;; ── case-try: chain where each binding feeds the next expression ──────────────

(fn pipeline [path transform]
  (case-try
    (io.open path :r) f
    (f:read :*a)       raw
    (transform raw)    processed
    processed
    (catch msg (values nil msg))))

;; ── match-try: parenthesised catch ───────────────────────────────────────────

(fn safe-index [t & keys]
  (match-try
    (. t (. keys 1))   v1
    (= :table (type v1)) true
    (. v1 (. keys 2))  v2
    v2
    (catch
      _ nil)))

;; ── pcall: multi-value destructuring ─────────────────────────────────────────

(fn guarded-call [f & args]
  (let [(ok result) (pcall f (table.unpack args))]
    (if ok result (error result))))

(fn attempt [f & args]
  (let [(ok result) (pcall f (table.unpack args))]
    (values ok result)))

(fn attempt-or [default f & args]
  (let [(ok result) (pcall f (table.unpack args))]
    (if ok result default)))

;; ── xpcall with traceback handler ────────────────────────────────────────────

(fn with-traceback [f & args]
  (let [handler (fn [err]
                  (.. err "\n" (debug.traceback "" 2)))
        (ok result) (xpcall f handler (table.unpack args))]
    (if ok result (error result))))

;; ── Nested case-try ───────────────────────────────────────────────────────────

(fn read-and-parse [path parser]
  (case-try
    (io.open path :r)  f
    (f:read :*a)        raw
    (case-try
      (parser raw)   parsed
      parsed
      (catch parse-err (values nil (.. "parse error: " (tostring parse-err)))))
    result
    result
    (catch io-err (values nil (.. "io error: " (tostring io-err))))))

;; ── Error objects (table errors) ─────────────────────────────────────────────

(fn make-error [kind message]
  {:kind kind :message message})

(fn try-with-structured-error [thunk]
  (let [(ok result) (pcall thunk)]
    (if ok
        [:ok result]
        (match result
          {:kind k :message m} [:err k m]
          msg                  [:err :unknown (tostring msg)]))))

;; ── Usage ─────────────────────────────────────────────────────────────────────

(local _r1 (try-open "/etc/hosts"))
(local _r2 (try-open-bare "/etc/hosts"))
(local _r3 (try-parse-number "42"))
(local _r4 (try-parse-number "not-a-number"))
(local _r5 (categorise-error (fn [] (error :not-found))))
(local _r6 (guarded-call + 1 2))
(local _r7 (attempt string.upper "hello"))
(local _r8 (attempt-or "default" string.upper "world"))
(local _r9 (try-with-structured-error (fn [] (make-error :io "disk full"))))
