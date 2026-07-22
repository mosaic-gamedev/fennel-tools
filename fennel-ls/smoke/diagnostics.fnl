;; diagnostics.fnl — intentional diagnostic triggers
;;
;; This file is a catalogue of cases the server should warn about.
;; Each warning is annotated with what the server should emit.
;; Use this to verify:
;;   1. The right warnings fire (no false negatives)
;;   2. No extra warnings fire on the clean sections (no false positives)
;;
;; Expected warning count: 10 warnings, 0 errors
;; Run:  fennel-ls check smoke/diagnostics.fnl

;; ── CLEAN: these should produce no warnings ───────────────────────────────────

(local pi 3.14159265358979)
(local tau (* 2 pi))

(fn circle-area [r]
  "Return the area of a circle with radius r."
  (* pi r r))

(fn circle-circumference [r]
  "Return the circumference of a circle with radius r."
  (* tau r))

;; var that IS mutated — no warning
(var counter 0)
(set counter (+ counter 1))
(set counter (+ counter 1))

;; params that ARE used — no warning
(fn add [a b] (+ a b))
(fn greet [name] (.. "hello " name))

;; _ prefix suppresses unused warnings by convention — no warning
(fn transform [x _metadata]
  (* x 2))

(each [_i v (ipairs [1 2 3])]
  (io.write (tostring v)))

;; correct arity calls — no warning
(local area  (circle-area 5))
(local circ  (circle-circumference 5))
(local sum   (add 1 2))
(local hello (greet "world"))
(local t2    (transform 7 {}))

;; varargs function — arity check should be suppressed entirely
(fn log [level & msgs]
  (io.write (.. "[" level "] " (table.concat msgs " ") "\n")))

(log :info "server" "started")
(log :warn "something" "might" "be" "wrong" "here")
(log :error)   ;; only level, no msgs — still valid because variadic

;; rest param with & — also variadic, no arity warning
(fn sum-all [& nums]
  (accumulate [total 0 _ n (ipairs nums)]
    (+ total n)))

(local big-sum (sum-all 1 2 3 4 5 6 7 8 9 10))

;; ── WARN: var never mutated (×2) ─────────────────────────────────────────────

;; WARN: `stale` is declared var but never set
(var stale "initial value")

;; WARN: `config-version` is declared var but never set
(var config-version 1)

(local _use-stale (.. stale (tostring config-version)))

;; ── WARN: unused local (×2) ──────────────────────────────────────────────────

(fn compute []
  ;; WARN: `intermediate` is bound but never used
  (let [intermediate (* 6 7)
        result       (+ 1 1)]
    result))

(fn with-scratch []
  ;; WARN: `scratch` is bound but never used
  (local scratch [1 2 3])
  42)

;; ── WARN: unused param (×2) ──────────────────────────────────────────────────

;; WARN: `b` is never referenced in body
(fn subtract-ignores-b [a b]
  a)

;; WARN: `options` is never referenced in body
(fn connect [host port options]
  (string.format "%s:%d" host port))

;; ── WARN: arity mismatch (×2) ────────────────────────────────────────────────

;; add takes exactly 2 params — calling with 1 should warn
(local bad-add-1 (add 99))

;; greet takes exactly 1 param — calling with 2 should warn
(local bad-greet (greet "alice" "bob"))

;; ── WARN: set on immutable binding (×1) ──────────────────────────────────────

(local immutable 100)
;; WARN: `immutable` is a local, cannot be set
(set immutable 200)

;; ── WARN: unknown identifier (×1) ────────────────────────────────────────────

;; WARN: `undefined-fn` is not in scope and not a builtin
(local bad-call (undefined-fn 1 2 3))

;; ── Clean tail: ensure no extra warnings bleed past the intentional ones ──────

(local _results
  [area circ sum hello t2 big-sum
   _use-stale (compute) (with-scratch)
   (subtract-ignores-b 1 2)
   (connect "localhost" 8080 {})
   bad-add-1 bad-greet bad-call])
