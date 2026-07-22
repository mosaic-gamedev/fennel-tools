;; iterators.fnl — collection comprehensions, &into, &until, threading macros
;;
;; Smoke test goals:
;;   - fcollect/faccumulate for-style loops
;;   - icollect/collect/accumulate/each iterator loops
;;   - &into modifier (merge into existing collection)
;;   - &until modifier (early termination)
;;   - as-> threading macro
;;   - ->, ->>, -?>, -?>> threading
;;   - doto chains, partial
;;
;; Expected: zero warnings

;; ── fcollect: basic for-style collect ────────────────────────────────────────

(fn range [lo hi]
  (fcollect [i lo hi] i))

(fn range-step [lo hi step]
  (fcollect [i lo hi step] i))

(fn squares [n]
  (fcollect [i 1 n] (* i i)))

(print (table.concat (range 1 5) " "))         ;; → 1 2 3 4 5
(print (table.concat (range-step 0 10 2) " ")) ;; → 0 2 4 6 8 10
(print (table.concat (squares 5) " "))          ;; → 1 4 9 16 25

;; ── faccumulate: for-style accumulate ────────────────────────────────────────

(fn sum-range [lo hi]
  (faccumulate [total 0 i lo hi]
    (+ total i)))

(fn product-range [lo hi]
  (faccumulate [prod 1 i lo hi]
    (* prod i)))

(fn build-map [n]
  (faccumulate [m {} i 1 n]
    (doto m (tset i (* i i)))))

(print (sum-range 1 100))    ;; → 5050
(print (product-range 1 5))  ;; → 120

;; ── faccumulate with &until — acc and loop var both visible in guard ──────────

(fn sum-up-to-limit [n limit]
  ;; acc (sum) and loop var (i) are BOTH visible in &until; no warning expected
  (faccumulate [sum 0 i 1 n &until (>= sum limit)]
    (+ sum i)))

(fn first-n-squares-under [n cap]
  (faccumulate [acc [] i 1 n &until (>= (* i i) cap)]
    (doto acc (table.insert (* i i)))))

(print (sum-up-to-limit 100 50))
(print (length (first-n-squares-under 20 100)))

;; ── fcollect with &into — extend existing collection ─────────────────────────

(fn extend-range [base lo hi]
  (fcollect [i lo hi &into base] i))

(local all-nums (extend-range [0] 1 5))  ;; → [0 1 2 3 4 5]
(print (length all-nums))

;; ── fcollect with &until — loop var visible in guard ────────────────────────

(fn collect-below [n cap]
  (fcollect [i 1 n &until (>= i cap)]
    i))

(fn take-while-squares-small [n threshold]
  (fcollect [i 1 n &until (> (* i i) threshold)]
    i))

(print (length (collect-below 100 10)))
(print (length (take-while-squares-small 20 50)))

;; ── icollect: iterator-based collect ─────────────────────────────────────────

(fn map-fn [f xs]
  (icollect [_ v (ipairs xs)] (f v)))

(fn filter-fn [pred xs]
  (icollect [_ v (ipairs xs)] (when (pred v) v)))

(fn keys-of [t]
  (icollect [k _ (pairs t)] k))

(print (table.concat (map-fn #(* $ 2) [1 2 3 4]) " "))   ;; → 2 4 6 8
(print (table.concat (filter-fn #(> $ 3) [1 2 3 4 5]) " ")) ;; → 4 5

;; ── icollect with &into ───────────────────────────────────────────────────────

(local base-list [1 2 3])
(local extended (icollect [_ v (ipairs [4 5 6]) &into base-list] v))
(print (length extended))  ;; → 6

;; ── icollect with &until ──────────────────────────────────────────────────────

(fn take-first [n xs]
  ;; &until tests the *result* of the body expression, but here we use
  ;; a counter via accumulate; simpler: limit via the loop binding
  (icollect [i v (ipairs xs) &until (> i n)] v))

(print (table.concat (take-first 3 [10 20 30 40 50]) " "))  ;; → 10 20 30

;; ── collect: key/value comprehension ─────────────────────────────────────────

(fn invert [t]
  (collect [k v (pairs t)] v k))

(fn index-by [xs key-fn]
  (collect [_ v (ipairs xs)] (key-fn v) v))

(fn map-values [f t]
  (collect [k v (pairs t)] k (f v)))

(local inv (invert {:a 1 :b 2 :c 3}))
(print (. inv 1) (. inv 2) (. inv 3))

;; ── collect with &into ────────────────────────────────────────────────────────

(fn merge-tables [a b]
  ;; b wins on conflicts
  (collect [k v (pairs b) &into (collect [k v (pairs a)] k v)]
    k v))

(local merged (merge-tables {:a 1 :b 2} {:b 99 :c 3}))
(print merged.a merged.b merged.c)  ;; → 1 99 3

;; ── accumulate ────────────────────────────────────────────────────────────────

(fn freq-count [xs]
  (accumulate [counts {} _ v (ipairs xs)]
    (doto counts (tset v (+ (or (. counts v) 0) 1)))))

(fn group-by-fn [key-fn xs]
  (accumulate [groups {} _ v (ipairs xs)]
    (let [k (key-fn v)]
      (when (= nil (. groups k)) (tset groups k []))
      (doto groups
        (tset k (doto (. groups k) (table.insert v)))))))

(local freqs (freq-count ["a" "b" "a" "c" "b" "a"]))
(print freqs.a freqs.b freqs.c)  ;; → 3 2 1

;; ── each with &until ──────────────────────────────────────────────────────────

(fn find-first [xs pred]
  (var result nil)
  (each [_ v (ipairs xs) &until result]
    (when (pred v) (set result v)))
  result)

(fn any? [xs pred]
  (var found false)
  (each [_ v (ipairs xs) &until found]
    (when (pred v) (set found true)))
  found)

(print (find-first [1 2 3 4 5] #(> $ 3)))  ;; → 4
(print (any? [1 2 3] #(= $ 2)))            ;; → true

;; ── -> and ->> threading ─────────────────────────────────────────────────────

;; ->> threads the accumulated value as the last argument
(fn sum-of-squares [xs]
  (accumulate [s 0 _ v (ipairs (icollect [_ v (ipairs xs)] (* v v)))]
    (+ s v)))

(print (sum-of-squares [1 2 3 4 5]))  ;; → 55

(local result
  (-> {:name "Alice" :score 95}
      (. :score)
      (* 2)
      tostring
      (.. "pts")))

(print result)  ;; → 190pts

;; chained nil-safe access
(local obj {:a {:b {:c 42}}})
(local val (-?> obj (. :a) (. :b) (. :c)))
(print val)  ;; → 42

(local missing (-?> obj (. :x) (. :y)))
(print missing)  ;; → nil

;; ── as-> threading ───────────────────────────────────────────────────────────

(fn normalize-str [s]
  (as-> s it
    (string.gsub it "%s+" " ")
    (string.lower it)
    (string.match it "^%s*(.-)%s*$")))

(fn pipeline [val & fns]
  (as-> val x
    (accumulate [v x _ f (ipairs fns)] (f v))))

(print (normalize-str "  Hello   World  "))
