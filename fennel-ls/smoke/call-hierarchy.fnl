;; call-hierarchy.fnl — dedicated test file for call hierarchy smoke tests

(fn helper [x]
  "Double x."
  (* x 2))

(fn caller-a [n]
  "Calls helper once."
  (helper n))

(fn caller-b [n]
  "Calls helper twice and sums."
  (+ (helper n) (helper (+ n 1))))
