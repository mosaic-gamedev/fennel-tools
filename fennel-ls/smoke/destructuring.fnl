;; destructuring.fnl — all binding and destructuring forms
;;
;; Smoke test goals:
;;   - sequential destructuring [a b c] and [head & tail]
;;   - table destructuring {:key name}, {: name}, {"key" name}, {name :key}
;;   - &as whole-table capture
;;   - multi-value (a b c) from pcall/values
;;   - nested patterns in let, match, fn params
;;   - loop variable destructuring in each/accumulate
;;   - let sequential scope: later bindings see earlier ones
;;
;; Expected: zero warnings

;; ── let sequential binding (each RHS sees previous bindings) ─────────────────

(let [a 1
      b (+ a 1)
      c (* b 3)
      d (string.format "a=%d b=%d c=%d" a b c)]
  (print d))

;; ── Sequential destructuring ──────────────────────────────────────────────────

(let [[x y z] [10 20 30]]
  (print (+ x y z)))

(let [[head & tail] [1 2 3 4 5]]
  (print head (length tail)))

(let [[a [b c] d] [1 [2 3] 4]]
  (print a b c d))

;; ── Table destructuring: all key forms ───────────────────────────────────────

;; {:key name}
(let [{:name name :age age} {:name "Alice" :age 30}]
  (string.format "%s is %d" name age))

;; {: name} shorthand (equivalent to {:name name})
(let [{: x : y} {:x 1 :y 2}]
  (+ x y))

;; {"string-key" name}
(let [{"content-type" ct "accept" acc} {"content-type" "text/html" "accept" "*/*"}]
  (string.format "%s %s" ct acc))

;; &as whole-table capture
(let [{:x px :y py &as point} {:x 3 :y 4}]
  (let [dist (math.sqrt (+ (* px px) (* py py)))]
    (print point.x point.y dist)))

;; nested table destructuring
(let [{:pos {:x x :y y} :name nm} {:pos {:x 1 :y 2} :name "origin"}]
  (print x y nm))

;; ── Multi-value destructuring from values / pcall ─────────────────────────────

(let [(a b c) (values 10 20 30)]
  (print (+ a b c)))

(let [(ok result) (pcall + 1 2)]
  (when ok (print result)))

(let [(ok err) (pcall error "boom")]
  (when (not ok) (print err)))

;; ── Destructuring in function params ─────────────────────────────────────────

(fn greet [{:name name :title title}]
  (string.format "%s %s" (or title "Mr/Ms") name))

(fn head+tail [[first & rest]]
  (values first rest))

(fn swap [[a b]]
  [b a])

(fn magnitude [{:x x :y y :z z}]
  (math.sqrt (+ (* x x) (* y y) (* (or z 0) (or z 0)))))

;; multi-param with mix of destructuring
(fn describe-range [[lo hi] {:step step :label label}]
  (string.format "%s: %d..%d step %d" (or label "range") lo hi (or step 1)))

;; ── Destructuring in each ─────────────────────────────────────────────────────

;; Sequential destructuring on each value
(each [_ [k v] (ipairs [[:a 1] [:b 2] [:c 3]])]
  (print k v))

;; Table destructuring on each value
(each [_ {:name nm :score sc} (ipairs [{:name "Alice" :score 95}
                                        {:name "Bob"   :score 87}])]
  (print nm sc))

;; ── Destructuring in accumulate ───────────────────────────────────────────────

(local total-score
  (accumulate [sum 0 _ {:score sc} (ipairs [{:name "A" :score 10}
                                             {:name "B" :score 20}])]
    (+ sum sc)))

(local freq
  (accumulate [counts {} _ word (ipairs ["a" "b" "a" "c" "b" "a"])]
    (doto counts (tset word (+ (or (. counts word) 0) 1)))))

;; ── Destructuring in collect ──────────────────────────────────────────────────

(local name-map
  (collect [_ {:id id :name nm} (ipairs [{:id 1 :name "Alice"} {:id 2 :name "Bob"}])]
    id nm))

;; ── match: all pattern forms ──────────────────────────────────────────────────

(fn classify [v]
  (match v
    nil                     :null
    true                    :true
    false                   :false
    (where n (= :number (type n)) (> n 0))  :positive-number
    (where n (= :number (type n)))           :non-positive-number
    (where s (= :string (type s)))           :string
    {:type t}               (.. :table/ t)
    [first & _]             (.. :sequence-starting/ (tostring first))
    _                       :other))

;; ── match: &as in table pattern ───────────────────────────────────────────────

(fn log-and-extract [{:level lvl :msg msg &as entry}]
  (io.write (string.format "[%s] %s\n" lvl msg))
  entry)

;; ── match: nested sequence pattern ───────────────────────────────────────────

(fn parse-pair [form]
  (match form
    [op left right] (values op left right)
    [op operand]    (values op operand nil)
    _               (values nil nil nil)))

;; ── Usage ─────────────────────────────────────────────────────────────────────

(print (greet {:name "Smith" :title "Dr"}))
(print (magnitude {:x 3 :y 4 :z 0}))
(print (describe-range [1 10] {:step 2 :label "evens"}))
(print total-score)
(print (. name-map 1))
(print (classify 42))
(print (classify "hello"))
(print (classify {:type "point"}))
(print (. freq "a"))
