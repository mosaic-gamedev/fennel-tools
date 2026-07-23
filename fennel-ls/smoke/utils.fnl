;; utils.fnl — module required by cross-file-refs.fnl

(fn greet [name]
  "Say hello to `name`."
  (print (.. "Hello, " name "!")))

(fn bye []
  "Say goodbye."
  (print "Goodbye!"))
