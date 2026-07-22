;; generators.fnl — coroutines, stateful iterators, string iteration
;;
;; Regression coverage for:
;;   - coroutine.wrap / coroutine.yield producer pattern
;;   - coroutine.create / coroutine.resume / coroutine.status
;;   - stateful iterator factory: var + closure returned as iterator
;;   - string.gmatch with single and multiple captures in each/icollect
;;   - custom iterators consumed by each / icollect / accumulate
;;   - generator pipeline: map, filter, take as lazy transformers
;;
;; Expected: zero warnings

;; ── coroutine.wrap producer ───────────────────────────────────────────────────

(fn range-gen [lo hi step]
  (coroutine.wrap
    (fn []
      (fcollect [i lo hi (or step 1)]
        (coroutine.yield i)))))

(fn repeat-gen [v n]
  (coroutine.wrap
    (fn []
      (fcollect [_ 1 n]
        (coroutine.yield v)))))

(fn cycle-gen [xs]
  (coroutine.wrap
    (fn []
      (while true
        (each [_ v (ipairs xs)]
          (coroutine.yield v))))))

;; consuming a coroutine.wrap generator with each
(fn collect-gen [gen n]
  (let [results []]
    (fcollect [_ 1 n]
      (table.insert results (gen))
      nil)
    results))

(print (table.concat (collect-gen (range-gen 1 5) 5) " "))   ;; 1 2 3 4 5
(print (table.concat (collect-gen (repeat-gen :x 3) 3) " ")) ;; x x x

;; collect from cycle (infinite — take first N)
(local cycler (cycle-gen [:a :b :c]))
(print (table.concat (collect-gen cycler 7) " "))  ;; a b c a b c a

;; ── coroutine.create / resume / status ───────────────────────────────────────

(fn make-counter-coro [lo hi]
  (coroutine.create
    (fn []
      (fcollect [i lo hi]
        (coroutine.yield i)))))

(fn drain [co]
  (let [results []]
    (while (not= (coroutine.status co) :dead)
      (let [(ok v) (coroutine.resume co)]
        (when (and ok v)
          (table.insert results v))))
    results))

(local co (make-counter-coro 10 14))
(print (coroutine.status co))       ;; suspended
(print (table.concat (drain co) " "))  ;; 10 11 12 13 14
(print (coroutine.status co))       ;; dead

;; ── Stateful iterator factory (var + closure) ─────────────────────────────────

(fn stateful-range [lo hi]
  (var i (- lo 1))
  (fn []
    (set i (+ i 1))
    (when (<= i hi)
      (values i i))))

(fn stateful-enumerate [xs]
  (var idx 0)
  (fn []
    (set idx (+ idx 1))
    (when (<= idx (length xs))
      (values idx (. xs idx)))))

(fn stateful-filter [pred iter]
  (fn []
    (var result nil)
    (while (not result)
      (let [v (iter)]
        (if (= nil v)
            (do (set result :__done) nil)
            (when (pred v)
              (set result v)))))
    (when (not= result :__done)
      result)))

;; use stateful iterator in each
(each [i v (stateful-enumerate [:a :b :c :d])]
  (io.write (string.format "%d:%s " i v)))
(io.write "\n")

;; use stateful iterator in accumulate
(local sum-range
  (accumulate [s 0 i _ (stateful-range 1 10)]
    (+ s i)))
(print sum-range)   ;; 55

;; icollect over stateful iterator
(local evens
  (icollect [_ v (stateful-range 1 10)]
    (when (= 0 (% v 2)) v)))
(print (table.concat evens " "))   ;; 2 4 6 8 10

;; ── string.gmatch: single-capture iterator ────────────────────────────────────

(fn words [s]
  (icollect [w (string.gmatch s "%S+")]
    w))

(fn ints-in [s]
  (icollect [n (string.gmatch s "%-?%d+")]
    (tonumber n)))

(fn lines [s]
  (icollect [l (string.gmatch (.. s "\n") "([^\n]*)\n")]
    l))

(print (table.concat (words "hello world foo bar") ", "))
(print (table.concat (icollect [n (ipairs (ints-in "x=1, y=-2, z=300"))] (tostring n)) " "))
(print (length (lines "a\nb\nc")))   ;; 3

;; ── string.gmatch: multi-capture iterator ─────────────────────────────────────

(fn parse-pairs [s]
  (collect [k v (string.gmatch s "(%w+)=(%w+)")]
    k v))

(fn parse-csv-row [s]
  (icollect [cell (string.gmatch (.. s ",") "([^,]*),")]
    cell))

(fn grep-matches [pattern s]
  (icollect [start stop (string.gmatch s "()(" .. pattern .. ")()")]
    {: start : stop}))

(local kv (parse-pairs "a=1 b=2 c=3"))
(print kv.a kv.b kv.c)

(let [row (parse-csv-row "alice,30,engineer")]
  (print (. row 1) (. row 2) (. row 3)))

;; each with multi-capture gmatch
(each [k v (string.gmatch "x=10,y=20,z=30" "(%w+)=(%d+)")]
  (io.write (string.format "%s→%s " k v)))
(io.write "\n")

;; accumulate with gmatch
(local total-from-string
  (accumulate [s 0 n (string.gmatch "1 2 3 4 5" "%d+")]
    (+ s (tonumber n))))
(print total-from-string)  ;; 15

;; ── Generator pipeline ────────────────────────────────────────────────────────

(fn gen-map [f gen]
  (fn [] (let [v (gen)] (when v (f v)))))

(fn gen-filter [pred gen]
  (fn []
    (var result nil)
    (while (not result)
      (let [v (gen)]
        (if (= nil v)
            (set result :__nil)
            (when (pred v)
              (set result v)))))
    (when (not= result :__nil) result)))

(fn gen-take [n gen]
  (var remaining n)
  (fn []
    (when (> remaining 0)
      (set remaining (- remaining 1))
      (gen))))

(fn gen-collect [gen]
  (let [results []]
    (var v (gen))
    (while v
      (table.insert results v)
      (set v (gen)))
    results))

(fn gen-fold [f init gen]
  (var acc init)
  (var v (gen))
  (while v
    (set acc (f acc v))
    (set v (gen)))
  acc)

;; pipeline: range → filter evens → map square → take 4
(local pipeline
  (-> (range-gen 1 20)
      (gen-filter #(= 0 (% $ 2)))
      (gen-map #(* $ $))
      (gen-take 4)
      gen-collect))

(print (table.concat pipeline " "))  ;; 4 16 36 64

;; pipeline: words from string → filter by length → uppercase
(local long-words
  (-> (coroutine.wrap
        (fn []
          (each [w (string.gmatch "the quick brown fox jumps over" "%a+")]
            (coroutine.yield w))))
      (gen-filter #(> (length $) 3))
      (gen-map string.upper)
      gen-collect))

(print (table.concat long-words " "))  ;; QUICK BROWN JUMPS OVER

;; fold: sum of squares of first 5 odd numbers
(local sum-odd-squares
  (gen-fold +
            0
            (-> (range-gen 1 100)
                (gen-filter #(= 1 (% $ 2)))
                (gen-map #(* $ $))
                (gen-take 5))))

(print sum-odd-squares)   ;; 1+9+25+49+81 = 165

;; ── Coroutine-based async simulation ─────────────────────────────────────────

(fn make-task [id duration]
  (coroutine.create
    (fn []
      (coroutine.yield (.. id ":start"))
      (fcollect [step 1 duration]
        (coroutine.yield (string.format "%s:step%d" id step)))
      (coroutine.yield (.. id ":done")))))

(fn run-scheduler [tasks]
  (var active (icollect [_ t (ipairs tasks)]
                (when (not= (coroutine.status t) :dead) t)))
  (let [log []]
    (while (> (length active) 0)
      (set active
           (icollect [_ co (ipairs active)]
             (let [(ok msg) (coroutine.resume co)]
               (when (and ok msg)
                 (table.insert log msg))
               (when (not= (coroutine.status co) :dead) co))))
    log)))


(local tasks [(make-task "A" 2) (make-task "B" 2)])
(let [log (run-scheduler tasks)]
  (each [_ msg (ipairs log)]
    (io.write msg " "))
  (io.write "\n"))
