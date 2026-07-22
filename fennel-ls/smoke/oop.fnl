;; oop.fnl — metatables, inheritance, metamethods
;;
;; Regression coverage for:
;;   - setmetatable, __index chains
;;   - metamethods: __tostring __add __mul __eq __lt __le __len __call __newindex
;;   - single-level and multi-level inheritance
;;   - method calls via colon (obj:method) and explicit (: obj :method args)
;;   - instance vs class variables
;;   - mixin / multiple-interface pattern
;;
;; Expected: zero warnings

;; ── Base class helper ─────────────────────────────────────────────────────────

(fn make-class [parent]
  (let [cls {}
        mt  {:__index cls}]
    (when parent
      (setmetatable cls {:__index parent}))
    (set cls._mt mt)
    (set cls.new (fn [self ...]
                   (let [inst (setmetatable {} mt)]
                     (when inst.init (inst:init ...))
                     inst)))
    cls))

;; ── Vec2: value type with arithmetic metamethods ──────────────────────────────

(local Vec2 (make-class nil))

(fn Vec2.init [self x y]
  (set self.x (or x 0))
  (set self.y (or y 0)))

(fn Vec2.length [self]
  (math.sqrt (+ (* self.x self.x) (* self.y self.y))))

(fn Vec2.dot [self other]
  (+ (* self.x other.x) (* self.y other.y)))

(fn Vec2.normalise [self]
  (let [len (self:length)]
    (if (> len 0)
        (Vec2:new (/ self.x len) (/ self.y len))
        (Vec2:new 0 0))))

(set Vec2._mt.__tostring
     (fn [self] (string.format "Vec2(%g, %g)" self.x self.y)))

(set Vec2._mt.__add
     (fn [a b] (Vec2:new (+ a.x b.x) (+ a.y b.y))))

(set Vec2._mt.__sub
     (fn [a b] (Vec2:new (- a.x b.x) (- a.y b.y))))

(set Vec2._mt.__mul
     (fn [a b]
       (if (= :number (type b))
           (Vec2:new (* a.x b) (* a.y b))
           (Vec2:new (* a.x b.x) (* a.y b.y)))))

(set Vec2._mt.__unm
     (fn [a] (Vec2:new (- a.x) (- a.y))))

(set Vec2._mt.__eq
     (fn [a b] (and (= a.x b.x) (= a.y b.y))))

(set Vec2._mt.__lt
     (fn [a b] (< (a:length) (b:length))))

(set Vec2._mt.__le
     (fn [a b] (<= (a:length) (b:length))))

(local v1 (Vec2:new 3 4))
(local v2 (Vec2:new 1 2))
(local v3 (+ v1 v2))
(local v4 (* v1 2))
(local v5 (- v1 v2))

(print (tostring v3))       ;; Vec2(4, 6)
(print (v1:length))         ;; 5.0
(print (v1:dot v2))         ;; 11
(print (tostring (v1:normalise)))
(print (= v1 (Vec2:new 3 4)))     ;; true
(print (< v2 v1))                 ;; true (shorter < longer)
(print (tostring (- v1)))          ;; Vec2(-3, -4)

;; explicit (: obj :method args) call syntax
(print (: v1 :dot v2))
(print (: v3 :length))

;; ── Animal hierarchy: three levels ───────────────────────────────────────────

(local Animal (make-class nil))

(fn Animal.init [self name sound]
  (set self.name name)
  (set self.sound sound)
  (set self.energy 100))

(fn Animal.speak [self]
  (string.format "%s says %s" self.name self.sound))

(fn Animal.eat [self amount]
  (set self.energy (+ self.energy (or amount 10)))
  self)

(fn Animal.rest [self]
  (set self.energy (math.min 100 (+ self.energy 20)))
  self)

(set Animal._mt.__tostring
     (fn [self] (string.format "Animal(%s)" self.name)))

;; Dog extends Animal
(local Dog (make-class Animal))

(fn Dog.init [self name]
  (Animal.init self name "woof")
  (set self.tricks []))

(fn Dog.learn [self trick]
  (table.insert self.tricks trick)
  self)

(fn Dog.perform [self]
  (if (= 0 (length self.tricks))
      (.. self.name " knows no tricks")
      (string.format "%s performs: %s"
                     self.name
                     (table.concat self.tricks ", "))))

(set Dog._mt.__tostring
     (fn [self]
       (string.format "Dog(%s, %d tricks)" self.name (length self.tricks))))

;; GuideDog extends Dog
(local GuideDog (make-class Dog))

(fn GuideDog.init [self name owner]
  (Dog.init self name)
  (set self.owner owner)
  (set self.certified false))

(fn GuideDog.certify [self]
  (set self.certified true)
  self)

(fn GuideDog.guide [self destination]
  (if self.certified
      (string.format "%s guides %s to %s" self.name self.owner destination)
      (string.format "%s is not yet certified" self.name)))

(local rex    (Dog:new "Rex"))
(local buddy  (GuideDog:new "Buddy" "Alice"))

(rex:learn "sit")
(rex:learn "stay")
(rex:learn "roll over")
(rex:eat 20)

(buddy:learn "heel")
(buddy:certify)

(print (rex:speak))
(print (rex:perform))
(print (tostring rex))
(print (buddy:guide "the park"))
(print (tostring buddy))
(print (Animal.speak rex))   ;; explicit super-method call

;; ── __newindex: intercept writes ─────────────────────────────────────────────

(fn make-readonly [t]
  (let [storage {}
        mt {}]
    (set mt.__index storage)
    (set mt.__newindex (fn [_ k _]
                         (error (.. "attempt to set read-only field: " (tostring k)))))
    (each [k v (pairs t)]
      (tset storage k v))
    (setmetatable {} mt)))

(local config (make-readonly {:host "localhost" :port 8080}))
(print config.host config.port)

;; ── __len metamethod ──────────────────────────────────────────────────────────

(fn make-bag []
  (let [items {}
        mt {}]
    (set mt.__index mt)
    (set mt.__len (fn [self] (length self._items)))
    (set mt._items items)
    (set mt.add (fn [self v]
                  (table.insert self._items v)
                  self))
    (set mt.get (fn [self i] (. self._items i)))
    (setmetatable {:_items items} mt)))

(local bag (make-bag))
(bag:add "a")
(bag:add "b")
(bag:add "c")
(print (length bag))   ;; 3 (via __len)
(print (bag:get 2))    ;; b

;; ── __call metamethod: callable table ────────────────────────────────────────

(fn make-memo [f]
  (let [cache {}
        mt {}]
    (set mt.__call (fn [self ...]
                     (let [key (table.concat [...] ",")]
                       (when (= nil (. cache key))
                         (tset cache key (f ...)))
                       (. cache key))))
    (setmetatable {:_cache cache} mt)))

(local memo-fib
  (make-memo (fn [n]
               (if (<= n 1) n
                   (+ n (- n 1))))))  ;; simplified; real fib needs self-ref

(print (memo-fib 5))
(print (memo-fib 5))  ;; cached

;; ── Mixin pattern ─────────────────────────────────────────────────────────────

(fn mix-into [cls & mixins]
  (each [_ mixin (ipairs mixins)]
    (each [k v (pairs mixin)]
      (when (= nil (. cls k))
        (tset cls k v)))))

(local Serialisable
  {:to-json (fn [self]
              (let [parts (icollect [k v (pairs self)]
                            (when (not= :function (type v))
                              (string.format "%q:%s" (tostring k) (tostring v))))]
                (.. "{" (table.concat parts ",") "}")))
   :to-string (fn [self]
                (table.concat
                  (icollect [k v (pairs self)]
                    (when (not= :function (type v))
                      (.. (tostring k) "=" (tostring v))))
                  " "))})

(local Comparable
  {:equals (fn [self other]
             (accumulate [eq true k v (pairs self)]
               (and eq (= v (. other k)))))})

(local Point (make-class nil))
(fn Point.init [self x y]
  (set self.x x)
  (set self.y y))

(mix-into Point Serialisable Comparable)

(local p1 (Point:new 1 2))
(local p2 (Point:new 1 2))
(local p3 (Point:new 3 4))

(print (p1:equals p2))   ;; true
(print (p1:equals p3))   ;; false
