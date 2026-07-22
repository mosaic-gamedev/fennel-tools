;; named-let.fnl — named let (tail-recursive loop form)
;;
;; Smoke test goals:
;;   - (let name [bindings] body) is the standard Fennel loop idiom
;;   - The loop name itself must be callable inside the body (recursive entry point)
;;   - Each binding in the vector must be in scope in the body
;;   - Multiple accumulators, destructuring bindings, string/table building
;;
;; Expected: zero warnings

;; ── Simplest form: single counter ────────────────────────────────────────────

(fn count-down [start]
(let loop [i start]
    (if (<= i 0)
        :done
        (loop (- i 1)))))

(print (count-down 5))

;; ── Two accumulators ──────────────────────────────────────────────────────────

(fn sum-to [n]
(let loop [i 1 total 0]
    (if (> i n)
        total
        (loop (+ i 1) (+ total i)))))

(print (sum-to 10))   ;; → 55

;; ── Fibonacci via named let ───────────────────────────────────────────────────

(fn fib-iter [n]
  (let loop [i n a 0 b 1]
    (if (= i 0)
        a
        (loop (- i 1) b (+ a b)))))

(print (fib-iter 10))  ;; → 55

;; ── Building a list (accumulate into a table) ─────────────────────────────────

(fn range [lo hi]
  (let loop [i hi acc []]
    (if (< i lo)
        acc
        (loop (- i 1) (doto acc (table.insert 1 i))))))

(print (table.concat (range 1 5) " "))  ;; → 1 2 3 4 5

;; ── Named let inside a fn — closure over outer param ─────────────────────────

(fn find-index [items pred]
  (let loop [i 1]
    (if (> i (length items))
        nil
        (if (pred (. items i))
            i
            (loop (+ i 1))))))

(print (find-index [10 20 30 40] #(> $ 25)))  ;; → 3

;; ── Named let with table destructuring in bindings ────────────────────────────

(fn flatten [t]
  (let loop [queue [t] acc []]
    (if (= 0 (length queue))
        acc
        (let [head (table.remove queue 1)]
          (if (= :table (type head))
              (do
                (each [_ v (ipairs head)]
                  (table.insert queue v))
                (loop queue acc))
              (do
                (table.insert acc head)
                (loop queue acc)))))))

(print (table.concat (flatten [1 [2 3] [4 [5 6]]]) " "))

;; ── String chunking via named let ─────────────────────────────────────────────

(fn split-by [s sep]
  (let loop [pos 1 parts []]
    (let [start (string.find s sep pos true)]
      (if start
          (loop (+ start (length sep))
                (doto parts (table.insert (string.sub s pos (- start 1)))))
          (doto parts (table.insert (string.sub s pos)))))))

(print (table.concat (split-by "a,b,c,d" ",") "|"))  ;; → a|b|c|d
