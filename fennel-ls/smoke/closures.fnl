;; closures.fnl — closures, lambdas, HashFn, varargs, recursion, higher-order
;;
;; Smoke test goals:
;;   - inner fn captures outer fn params
;;   - closure over mutable var (set inside inner fn)
;;   - recursive named fn (self-reference in body)
;;   - lambda / λ synonyms
;;   - HashFn: $, $1–$9, $... rest
;;   - varargs ... accessible in fn body
;;   - & rest param accessible in fn body
;;   - fn used before definition via var+set trick (mutual recursion)
;;   - fn returned from fn (currying)
;;   - fn stored in table (method-like)
;;
;; Expected: zero warnings

;; ── Inner fn captures outer params ───────────────────────────────────────────

(fn make-adder [n]
  (fn [x] (+ n x)))

(fn make-multiplier [factor]
  (fn [& vals]
    (icollect [_ v (ipairs vals)] (* factor v))))

(fn make-range-check [lo hi]
  (fn [x]
    (and (>= x lo) (<= x hi))))

;; ── Closure over mutable var ──────────────────────────────────────────────────

(fn make-counter [start step]
  (var n start)
  {:next  (fn [] (let [v n] (set n (+ n step)) v))
   :reset (fn [] (set n start))
   :peek  (fn [] n)})

(fn make-accumulator []
  (var total 0)
  (var count 0)
  {:add  (fn [v] (set total (+ total v)) (set count (+ count 1)))
   :mean (fn [] (if (= count 0) 0 (/ total count)))
   :sum  (fn [] total)})

;; ── Recursive named fns ───────────────────────────────────────────────────────

(fn factorial [n]
  (if (<= n 1) 1 (* n (factorial (- n 1)))))

(fn fib [n]
  (if (<= n 1)
      n
      (+ (fib (- n 1)) (fib (- n 2)))))

(fn deep-count [pred t]
  "Count all leaf values in nested tables satisfying pred."
  (accumulate [n 0 _ v (pairs t)]
    (if (= :table (type v))
        (+ n (deep-count pred v))
        (if (pred v) (+ n 1) n))))

;; ── lambda / λ ────────────────────────────────────────────────────────────────

(local square  (lambda [x] (* x x)))
(local cube    (λ [x] (* x x x)))
(local negate  (lambda [x] (- x)))
(local id      (λ [x] x))

;; lambda with destructuring
(local add-pair (lambda [[a b]] (+ a b)))
(local key-of   (lambda [{:key k}] k))

;; ── HashFn ────────────────────────────────────────────────────────────────────

;; $ is alias for $1 (first argument)
(local inc    #(+ $ 1))
(local double #(* $ 2))
(local neg    #(- $))

;; explicit numbered args
(local add    #(+ $1 $2))
(local sub    #(- $1 $2))
(local clamp  #(math.max $1 (math.min $2 $3)))

;; $... for rest args — passed to table.pack / ipairs
(local sum-hf #(accumulate [t 0 _ v (ipairs [$...])] (+ t v)))

;; nested HashFn
(local pipe2  #((#(* $ 2)) $))

;; ── Varargs ... in fn body ────────────────────────────────────────────────────

(fn sum [...]
  (accumulate [total 0 _ v (ipairs [...])]
    (+ total v)))

(fn product [...]
  (accumulate [p 1 _ v (ipairs [...])]
    (* p v)))

(fn join [sep ...]
  (table.concat [...] sep))

(fn count-args [...]
  (select "#" ...))

(fn first-arg [first ...]
  (let [rest-count (select "#" ...)]
    (values first rest-count)))

;; varargs forwarding
(fn logged-call [name f ...]
  (io.write (string.format "calling %s\n" name))
  (f ...))

;; ── & rest param in fn body ───────────────────────────────────────────────────

(fn min-of [first & rest]
  (accumulate [m first _ v (ipairs rest)]
    (if (< v m) v m)))

(fn max-of [first & rest]
  (accumulate [m first _ v (ipairs rest)]
    (if (> v m) v m)))

(fn zip-with [f a-list b-list]
  (icollect [i a (ipairs a-list)]
    (f a (. b-list i))))

;; ── Mutual recursion via var+set ──────────────────────────────────────────────

(var is-even? nil)
(var is-odd?  nil)

(set is-even? (fn [n]
                (if (= n 0) true (is-odd? (- n 1)))))

(set is-odd? (fn [n]
               (if (= n 0) false (is-even? (- n 1)))))

;; ── Currying / fn returning fn ───────────────────────────────────────────────

(fn curry2 [f]
  (fn [a] (fn [b] (f a b))))

(fn curry3 [f]
  (fn [a] (fn [b] (fn [c] (f a b c)))))

(local add-curried   ((curry2 +) 10))
(local clamp-curried ((curry3 math.max) 0))

;; ── Fns stored in tables ──────────────────────────────────────────────────────

(local Vec2
  (let [mt {}]
    (set mt.__index mt)
    (set mt.new    (fn [x y] (setmetatable {:x x :y y} mt)))
    (set mt.add    (fn [self other] (mt.new (+ self.x other.x) (+ self.y other.y))))
    (set mt.scale  (fn [self s] (mt.new (* self.x s) (* self.y s))))
    (set mt.length (fn [self] (math.sqrt (+ (* self.x self.x) (* self.y self.y)))))
    mt))

;; ── Usage ─────────────────────────────────────────────────────────────────────

(local add5 (make-adder 5))
(local by3  (make-multiplier 3))
(local in-range? (make-range-check 1 100))

(local ctr (make-counter 0 1))
((. ctr :next))
((. ctr :next))

(local acc (make-accumulator))
((. acc :add) 10)
((. acc :add) 20)

(print (factorial 10))
(print (fib 10))
(print (sum 1 2 3 4 5))
(print (product 2 3 4))
(print (join ", " "a" "b" "c"))
(print (min-of 5 3 8 1 9))
(print (max-of 5 3 8 1 9))
(print (is-even? 4))
(print (is-odd? 7))
(print (add5 3))
(print (inc 41))
(print (add 3 4))
(print (clamp 0 100 150))
(print (sum-hf 1 2 3 4 5))
(print (square 7))
(print (add-pair [3 4]))
(print (key-of {:key "hello"}))
(print (add-curried 5))
(print (clamp-curried 50))
(print (deep-count (fn [v] (= :number (type v))) {:a 1 :b {:c 2 :d "x"}}))
(print (zip-with + [1 2 3] [4 5 6]))

(local v1 (Vec2.new 3 4))
(local v2 (Vec2.new 1 2))
(local v3 (v1:add v2))
(print (v3:length))
