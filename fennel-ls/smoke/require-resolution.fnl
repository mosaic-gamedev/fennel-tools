;; require-resolution.fnl — cross-file require smoke test
;;
;; Smoke test goals (open this file with the LSP running, workspace root = smoke/):
;;
;;   HOVER
;;     - hover on `geometry.vec2`        → shows "(fn vec2 [x y])" + docstring
;;     - hover on `geometry.rect`        → shows "(fn rect [x y w h])" + docstring
;;     - hover on `geometry.rect-center` → shows "(fn rect-center [r])" + docstring
;;
;;   GO-TO-DEFINITION
;;     - go-to-def on `geometry.vec2`        → jumps to vec2 in geometry.fnl
;;     - go-to-def on `geometry.vec2-add`    → jumps to vec2-add in geometry.fnl
;;     - go-to-def on `geometry.rect`        → jumps to rect in geometry.fnl
;;     - go-to-def on `:geometry` string arg → jumps to geometry.fnl (file open)
;;
;;   COMPLETION
;;     - type `geometry.` and trigger completion → list includes vec2, vec2-add,
;;       vec2-scale, vec2-dot, vec2-length, rect, rect-contains?, rect-center
;;
;;   DIAGNOSTICS
;;     - NO "unknown identifier" warnings for any `geometry.*` access below
;;     - NO spurious warnings elsewhere in this file

(local geometry (require :geometry))

;; ── Basic construction ────────────────────────────────────────────────────────

(local origin (geometry.vec2 0 0))
(local tip    (geometry.vec2 3 4))

;; ── Arithmetic ────────────────────────────────────────────────────────────────

(local sum     (geometry.vec2-add origin tip))
(local scaled  (geometry.vec2-scale tip 2))
(local dot-val (geometry.vec2-dot origin tip))
(local len     (geometry.vec2-length tip))       ;; should be 5.0

;; ── Rectangles ───────────────────────────────────────────────────────────────

(local viewport (geometry.rect 0 0 800 600))
(local center   (geometry.rect-center viewport))

;; ── Predicate ────────────────────────────────────────────────────────────────

(local inside? (geometry.rect-contains? viewport 400 300))   ;; true
(local outside? (geometry.rect-contains? viewport 900 300))  ;; false

;; ── Use the results to suppress unused-local warnings ────────────────────────

(fn describe-point [v]
  "Return a string description of a vec2."
  (string.format "(%.2f, %.2f)" v.x v.y))

(local _summary
  (string.format
    "sum=%s len=%.2f inside=%s outside=%s center=%s scaled=%s dot=%.2f"
    (describe-point sum)
    len
    (tostring inside?)
    (tostring outside?)
    (describe-point center)
    (describe-point scaled)
    dot-val))
