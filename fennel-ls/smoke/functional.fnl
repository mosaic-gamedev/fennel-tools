;; functional.fnl — functional programming utilities
;;
;; Smoke test goals:
;;   - hover on fn names shows signature + docstring
;;   - go-to-def on all local references resolves correctly
;;   - no spurious unknown-identifier warnings (all refs are in scope)
;;   - arity warnings fire for the intentional bad call at the bottom
;;   - unused-param warnings fire on `_ignored`

;; ── Core combinators ──────────────────────────────────────────────────────────

(fn map [f t]
  "Apply f to every element of sequential table t, returning a new table."
  (icollect [_ v (ipairs t)] (f v)))

(fn filter [pred t]
  "Return a new table containing only elements of t for which pred returns truthy."
  (icollect [_ v (ipairs t)]
    (when (pred v) v)))

(fn reduce [f init t]
  "Left-fold t with binary function f, starting from init."
  (accumulate [acc init _ v (ipairs t)]
    (f acc v)))

(fn each! [f t]
  "Call f on every element of t for side effects. Returns nil."
  (each [_ v (ipairs t)]
    (f v)))

(fn zip [a b]
  "Zip two sequential tables into a table of pairs. Stops at the shorter one."
  (let [result []]
    (each [idx v (ipairs a)]
      (let [bv (. b idx)]
        (when bv
          (table.insert result [v bv]))))
    result))

(fn flatten [t]
  "Flatten one level of nesting from a table of tables."
  (accumulate [out [] _ inner (ipairs t)]
    (do
      (each [_ v (ipairs inner)]
        (table.insert out v))
      out)))

(fn take [n t]
  "Return the first n elements of t."
  (icollect [i v (ipairs t)]
    (when (<= i n) v)))

(fn drop [n t]
  "Return all but the first n elements of t."
  (icollect [i v (ipairs t)]
    (when (> i n) v)))

(fn partition [pred t]
  "Split t into two tables: [matching non-matching]."
  (let [yes []
        no  []]
    (each [_ v (ipairs t)]
      (if (pred v)
          (table.insert yes v)
          (table.insert no v)))
    (values yes no)))

;; ── Function composition ──────────────────────────────────────────────────────

(fn compose [& fns]
  "Return a function that applies fns right-to-left."
  (fn [x]
    (accumulate [v x _ f (ipairs (-> fns
                                     table.pack
                                     (doto (tset :n nil))))]
      (f v))))

(fn pipe [& fns]
  "Return a function that applies fns left-to-right (reverse compose)."
  (let [n        (length fns)
        reversed []]
    (for idx 1 n 1
      (table.insert reversed (. fns (- n idx -1))))
    (compose (table.unpack reversed))))

(fn partial [f & args]
  "Partially apply f with args prepended to future calls."
  (fn [& rest]
    (f (table.unpack args) (table.unpack rest))))

(fn memoize [f]
  "Wrap f so repeated calls with equal string-coerced args return cached values."
  (let [cache {}]
    (fn [& args]
      (let [key (table.concat (icollect [_ a (ipairs args)] (tostring a)) ",")]
        (when (= nil (. cache key))
          (tset cache key (f (table.unpack args))))
        (. cache key)))))

;; ── Table utilities ───────────────────────────────────────────────────────────

(fn keys [t]
  "Return all keys of t as a sequential table (order unspecified)."
  (icollect [k _ (pairs t)] k))

(fn vals [t]
  "Return all values of t as a sequential table (order unspecified)."
  (icollect [_ v (pairs t)] v))

(fn assoc [t & kvs]
  "Return a shallow copy of t with extra key-value pairs merged in."
  (let [out {}]
    (each [k v (pairs t)]
      (tset out k v))
    (for i 1 (length kvs) 2
      (tset out (. kvs i) (. kvs (+ i 1))))
    out))

(fn dissoc [t & ks]
  "Return a shallow copy of t with the given keys removed."
  (let [out {}
        remove-set (collect [_ k (ipairs ks)] k true)]
    (each [k v (pairs t)]
      (when (not (. remove-set k))
        (tset out k v)))
    out))

(fn group-by [f t]
  "Group elements of t by the return value of f."
  (accumulate [groups {} _ v (ipairs t)]
    (let [k (f v)]
      (when (= nil (. groups k))
        (tset groups k []))
      (table.insert (. groups k) v)
      groups)))

;; ── Usage examples (exercises hover + go-to-def) ─────────────────────────────

(local nums [1 2 3 4 5 6 7 8 9 10])

(local evens   (filter (partial = 0) nums))   ;; wrong arity on =: smoke arity check
(local doubled (map (partial * 2) nums))
(local total   (reduce + 0 nums))

(local [small large] [(take 3 nums) (drop 7 nums)])

(local words ["fennel" "lua" "lisp" "rust"])
(local lengths (map length words))
(local by-len  (group-by length words))

;; compose: uppercase then reverse a string
(local transform
  (compose
    (fn [s] (string.reverse s))
    (fn [s] (string.upper s))))

(local transformed (map transform words))

;; memoized fibonacci to exercise closure + recursion
(var fib nil)
(set fib
  (memoize
    (fn [n]
      (if (<= n 1)
          n
          (+ (fib (- n 1)) (fib (- n 2)))))))

(local fib-10 (fib 10))

;; partition odds and evens
(local [odds evens-2] (partition (fn [n] (= 1 (% n 2))) nums))

;; assoc / dissoc round-trip
(local base   {:a 1 :b 2 :c 3})
(local added  (assoc base :d 4 :e 5))
(local pruned (dissoc added :b :d))

;; pipe: double then stringify each number
(local stringify-doubled
  (pipe (partial map (partial * 2))
        (partial map tostring)))

(local result (stringify-doubled [1 2 3]))

;; ── Intentional diagnostics (verify the server catches these) ────────────────

;; WARN: unused local (_ignored is never referenced)
(fn demonstrating-unused-param [x _ignored]
  (* x x))

;; WARN: var never mutated (should suggest changing to local)
(var immutable-var 42)

(local final-answer immutable-var)
