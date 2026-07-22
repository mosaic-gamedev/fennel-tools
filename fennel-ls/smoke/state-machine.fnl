;; state-machine.fnl — generic finite state machine with event dispatch
;;
;; Smoke test goals:
;;   - deeply nested scopes (closures inside closures)
;;   - match/case pattern matching
;;   - table destructuring in multiple forms
;;   - with-open resource management
;;   - method calls (obj:method multisym)
;;   - varargs & rest params
;;   - go-to-def crosses let/do scope boundaries correctly

;; ── State machine constructor ─────────────────────────────────────────────────

(fn make-machine [initial-state transitions]
  "Create a finite state machine.

  transitions is a table of {from-state {event to-state-or-fn}}.
  When to-state-or-fn is a function it receives the machine and event data
  and must return the next state name."
  (var current initial-state)
  (let [history    []
        listeners  {}
        on-error   (fn [state event]
                     (string.format "no transition from %s on %s"
                                    (tostring state)
                                    (tostring event)))]

    (fn transition! [event & data]
      (let [from-map (. transitions current)
            handler  (and from-map (. from-map event))]
        (if handler
            (let [next-state (if (= :function (type handler))
                                (handler current event (table.unpack data))
                                handler)]
              (table.insert history {:from current :to next-state :event event})
              (set current next-state)
              ;; fire listeners
              (each [_ cb (ipairs (or (. listeners event) []))]
                (cb current event (table.unpack data)))
              next-state)
            (error (on-error current event)))))

    (fn on [event cb]
      (when (= nil (. listeners event))
        (tset listeners event []))
      (table.insert (. listeners event) cb))

    (fn state [] current)

    (fn in? [& states]
      (accumulate [found false _ s (ipairs states)]
        (or found (= current s))))

    (fn reset! []
      (set current initial-state)
      (while (> (length history) 0)
        (table.remove history)))

    {:transition! transition!
     :on          on
     :state       state
     :in?         in?
     :reset!      reset!
     :history     history}))

;; ── Traffic light example ─────────────────────────────────────────────────────

(local traffic-transitions
  {:red    {:tick :green}
   :green  {:tick :yellow
            :emergency :red}
   :yellow {:tick :red
            :emergency :red}})

(local light (make-machine :red traffic-transitions))

;; Register a listener to log transitions
((. light :on) :tick
  (fn [new-state _event]
    (io.write (string.format "light → %s\n" new-state))))

;; Advance through a full cycle
((. light :transition!) :tick)   ;; red → green
((. light :transition!) :tick)   ;; green → yellow
((. light :transition!) :tick)   ;; yellow → red

;; ── Promise-like async state machine ─────────────────────────────────────────

(fn make-promise []
  "A minimal promise/deferred with pending/fulfilled/rejected states."
  (let [callbacks {:fulfilled [] :rejected []}
        transitions {:pending   {:resolve :fulfilled
                                 :reject  :rejected}
                     :fulfilled {}
                     :rejected  {}}
        machine (make-machine :pending transitions)]

    (var stored-value nil)

    (fn resolve! [value]
      (set stored-value value)
      ((. machine :transition!) :resolve value)
      (each [_ cb (ipairs (. callbacks :fulfilled))]
        (cb value)))

    (fn reject! [reason]
      (set stored-value reason)
      ((. machine :transition!) :reject reason)
      (each [_ cb (ipairs (. callbacks :rejected))]
        (cb reason)))

    (fn and-then [on-fulfilled on-rejected]
      (match ((. machine :state))
        :fulfilled (on-fulfilled stored-value)
        :rejected  (when on-rejected (on-rejected stored-value))
        :pending   (do
                     (table.insert (. callbacks :fulfilled) on-fulfilled)
                     (when on-rejected
                       (table.insert (. callbacks :rejected) on-rejected)))))

    {:resolve!  resolve!
     :reject!   reject!
     :and-then  and-then
     :state     (. machine :state)}))

;; ── Pattern matching showcase ─────────────────────────────────────────────────

(fn classify-event [event]
  "Classify an LSP-style event table by its method field."
  (match event
    {:method "initialize"}           :init
    {:method "textDocument/hover"}   :hover
    {:method (where m (= 0 (m:find "textDocument/")))} :text-doc
    {:method m}                      (.. :unknown/ m)
    nil                              :null-event
    _                                :malformed))

(fn handle-result [result]
  "Destructure a (ok value) / (err msg) tagged union."
  (match result
    [:ok  value]  (string.format "success: %s" (tostring value))
    [:err reason] (string.format "error: %s"   (tostring reason))
    _             "unexpected result shape"))

;; ── Recursive data processing ─────────────────────────────────────────────────

;; Helper used by path-set; defined first so it is in scope below
(fn assoc [t k v]
  (let [out {}]
    (each [ck cv (pairs t)]
      (tset out ck cv))
    (tset out k v)
    out))

(fn deep-merge [base override]
  "Recursively merge override into base, preferring override on conflicts."
  (let [out {}]
    (each [k v (pairs base)]
      (tset out k v))
    (each [k v (pairs override)]
      (tset out k
        (if (and (= :table (type v))
                 (= :table (type (. base k))))
            (deep-merge (. base k) v)
            v)))
    out))

(fn walk [f t]
  "Walk a nested table, applying f to every non-table leaf value."
  (let [out {}]
    (each [k v (pairs t)]
      (tset out k
        (if (= :table (type v))
            (walk f v)
            (f v))))
    out))

(fn path-get [t & keys]
  "Traverse t following the given key path; returns nil if any key is missing."
  (accumulate [node t _ k (ipairs keys)]
    (and node (. node k))))

(fn path-set [t keys value]
  "Return a new table with value set at the nested key path."
  (match keys
    [k]       (assoc t k value)
    [k & rest]
    (let [inner (or (. t k) {})]
      (assoc t k (path-set inner rest value)))))

;; ── with-open smoke ───────────────────────────────────────────────────────────

(fn read-config [path]
  "Read and return raw text from path, or nil on error."
  (let [(ok result) (pcall
                      (fn []
                        (with-open [f (io.open path :r)]
                          (f:read :*a))))]
    (if ok result nil)))

;; ── Case-try / error handling ─────────────────────────────────────────────────

(fn safe-divide [a b]
  (case-try
    (= 0 b)       false
    (/ a b)       result
    (catch
      false (values nil "division by zero")
      _     (values nil "unexpected error"))))

;; ── Intentional diagnostics ───────────────────────────────────────────────────

;; WARN: var never mutated
(var unused-counter 0)

;; WARN: unused local (never referenced after binding)
(fn process [items _ctx]
  (each [_ item (ipairs items)]
    (io.write (tostring item))))

(local _final (process [1 2 3] {}))
