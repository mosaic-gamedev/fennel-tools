;; geometry.fnl — module exported and required by require-resolution.fnl
;;
;; Smoke test goals (open this file directly):
;;   - hover on fn names shows their signature + docstring
;;   - go-to-def within this file resolves locally
;;   - no spurious warnings

(fn vec2 [x y]
  "Construct a 2-D vector table."
  {:x x :y y})

(fn vec2-add [a b]
  "Add two vec2 tables component-wise, returning a new vec2."
  (vec2 (+ a.x b.x) (+ a.y b.y)))

(fn vec2-scale [v s]
  "Scale vec2 `v` by scalar `s`."
  (vec2 (* v.x s) (* v.y s)))

(fn vec2-dot [a b]
  "Dot product of two vec2 tables."
  (+ (* a.x b.x) (* a.y b.y)))

(fn vec2-length [v]
  "Euclidean length of vec2 `v`."
  (math.sqrt (vec2-dot v v)))

(fn rect [x y w h]
  "Construct an axis-aligned rectangle."
  {:x x :y y :w w :h h})

(fn rect-contains? [r px py]
  "Return true if point (px, py) lies inside rectangle `r`."
  (and (>= px r.x) (< px (+ r.x r.w))
       (>= py r.y) (< py (+ r.y r.h))))

(fn rect-center [r]
  "Return the center of rectangle `r` as a vec2."
  (vec2 (+ r.x (/ r.w 2)) (+ r.y (/ r.h 2))))
