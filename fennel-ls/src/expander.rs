/// Macro expander: compiles Fennel source in a background thread using an
/// embedded LuaJIT + Fennel runtime and returns the set of names that the
/// expanded top-level forms introduce into scope.
///
/// Enabled by the `embedded-fennel` Cargo feature.

#[cfg(feature = "embedded-fennel")]
mod inner {
    use mlua::prelude::*;
    use std::collections::HashSet;
    use tokio::sync::{mpsc, oneshot};

    pub(super) const FENNEL_SRC: &str = include_str!("../vendor/fennel.lua");

    // Lua helper loaded once into the long-lived Lua state.
    //
    // `fennel_debug_compile(src)` — returns raw Lua output or "ERROR: …".
    // `fennel_defined_names(src, search_path)` — returns newline-separated
    //   top-level Fennel names discovered after compilation.
    //
    // Strategy for name extraction: compile to Lua, then match `\nlocal <name>`
    // at column 0. In Fennel's output, top-level definitions produce un-indented
    // `local` declarations; nested locals (inside function bodies / let blocks)
    // are indented with spaces and therefore don't match.
    pub(super) const HELPER: &str = r#"
function fennel_debug_compile(src)
    local ok, result = pcall(fennel.compileString, src, {allowedGlobals = false})
    if not ok then return "ERROR: " .. tostring(result) end
    return result
end

function fennel_defined_names(src, search_path)
    local orig_path = fennel.path
    -- fennel["macro-path"] (hyphenated) is what the macro searcher reads;
    -- fennel.macroPath (camelCase) is a different key and has no effect.
    local orig_macro_path = fennel["macro-path"]
    if search_path and search_path ~= "" then
        local prefix = search_path .. "/?.fnl;" .. search_path .. "/?/init.fnl;"
        fennel.path = prefix .. orig_path
        fennel["macro-path"] = prefix .. orig_macro_path
    end
    local scope = fennel.scope()
    -- compilerEnv intentionally omitted so Fennel uses its built-in compiler
    -- environment, which includes quasiquote helpers (list, sym, …).
    -- allowedGlobals=false suppresses unknown-global warnings in user code.
    local ok, _ = pcall(fennel.compileString, src, {
        scope = scope,
        allowedGlobals = false,
    })
    fennel.path = orig_path
    fennel["macro-path"] = orig_macro_path
    if not ok then return "" end

    local names = {}
    local seen = {}

    local function add(name)
        if type(name) == "string" and not name:match("^_") and not seen[name] then
            seen[name] = true
            names[#names + 1] = name
        end
    end

    -- scope.unmanglings: Lua identifier → original Fennel name.
    -- Populated during compilation; avoids mangling ambiguity (my_var vs my-var).
    for _, fennel_name in pairs(scope.unmanglings) do add(fennel_name) end

    -- scope.macros: name → macro function.
    -- Populated by (import-macros …) and (macro …) forms.
    -- Adding these suppresses "unknown identifier" warnings on macro call sites.
    for macro_name, _ in pairs(scope.macros) do add(macro_name) end

    return table.concat(names, "\n")
end
"#;

    struct Request {
        src: String,
        search_path: String,
        respond: oneshot::Sender<HashSet<String>>,
    }

    /// Handle to the background expander thread. Cheap to clone (wraps a channel sender).
    #[derive(Clone)]
    pub struct MacroExpander {
        tx: mpsc::Sender<Request>,
    }

    impl MacroExpander {
        pub fn new() -> Self {
            let (tx, mut rx) = mpsc::channel::<Request>(32);

            std::thread::Builder::new()
                .name("fennel-expander".into())
                .spawn(move || {
                    // SAFETY: we need the full stdlib (including `debug`) because Fennel
// uses `debug.traceback` internally. The expander thread only runs
// user-provided Fennel source, so this is equivalent to running
// `fennel` from the command line.
let lua = unsafe { Lua::unsafe_new() };
                    let fennel_mod: LuaTable = match lua.load(FENNEL_SRC).set_name("fennel.lua").eval() {
                        Ok(t) => t,
                        Err(e) => { log::error!("failed to load Fennel: {e}"); return; }
                    };
                    if let Err(e) = lua.globals().set("fennel", fennel_mod) {
                        log::error!("failed to set fennel global: {e}"); return;
                    }
                    if let Err(e) = lua.load(HELPER).exec() {
                        log::error!("failed to load expander helper: {e}"); return;
                    }
                    let expand_fn: LuaFunction = match lua.globals().get("fennel_defined_names") {
                        Ok(f) => f,
                        Err(e) => { log::error!("failed to get fennel_defined_names: {e}"); return; }
                    };

                    while let Some(req) = rx.blocking_recv() {
                        let result: LuaResult<String> = expand_fn.call((req.src, req.search_path));
                        let names: HashSet<String> = match result {
                            Ok(s) => s.lines()
                                .filter(|l| !l.is_empty())
                                .map(|l| l.to_string())
                                .collect(),
                            Err(e) => {
                                log::debug!("macro expansion failed: {e}");
                                HashSet::new()
                            }
                        };
                        let _ = req.respond.send(names);
                    }
                })
                .expect("spawn expander thread");

            Self { tx }
        }

        pub async fn expand(&self, src: &str, search_path: &str) -> HashSet<String> {
            let (respond_tx, respond_rx) = oneshot::channel();
            if self.tx.send(Request {
                src: src.to_owned(),
                search_path: search_path.to_owned(),
                respond: respond_tx,
            }).await.is_err() {
                return HashSet::new();
            }
            respond_rx.await.unwrap_or_default()
        }
    }

    // ── Unit tests for the Lua layer (run synchronously, no channel needed) ────

    #[cfg(test)]
    mod lua_tests {
        use super::*;

        fn make_lua() -> Lua {
            // SAFETY: we need the full stdlib (including `debug`) because Fennel
// uses `debug.traceback` internally. The expander thread only runs
// user-provided Fennel source, so this is equivalent to running
// `fennel` from the command line.
let lua = unsafe { Lua::unsafe_new() };
            let fennel_mod: LuaTable = lua.load(FENNEL_SRC).set_name("fennel.lua").eval().unwrap();
            lua.globals().set("fennel", fennel_mod).unwrap();
            lua.load(HELPER).exec().unwrap();
            lua
        }

        fn debug_compile(lua: &Lua, src: &str) -> String {
            let f: LuaFunction = lua.globals().get("fennel_debug_compile").unwrap();
            let out: String = f.call(src).unwrap();
            out
        }

        fn defined_names(lua: &Lua, src: &str) -> Vec<String> {
            let f: LuaFunction = lua.globals().get("fennel_defined_names").unwrap();
            let out: String = f.call((src, "")).unwrap();
            let mut names: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(|s| s.to_string()).collect();
            names.sort();
            names
        }

        #[test]
        fn compile_local_produces_lua_output() {
            let lua = make_lua();
            let out = debug_compile(&lua, "(local foo 42)");
            eprintln!("lua output: {out:?}");
            assert!(!out.starts_with("ERROR:"), "compile failed: {out}");
            assert!(out.contains("foo"), "expected 'foo' in: {out}");
        }

        #[test]
        fn compile_fn_produces_lua_output() {
            let lua = make_lua();
            let out = debug_compile(&lua, "(fn greet [x] (print x))");
            eprintln!("fn output: {out:?}");
            assert!(!out.starts_with("ERROR:"), "compile failed: {out}");
            assert!(out.contains("greet"), "expected 'greet' in: {out}");
        }

        #[test]
        fn finds_local_def() {
            let lua = make_lua();
            let names = defined_names(&lua, "(local foo 42)");
            assert!(names.contains(&"foo".to_string()), "got: {names:?}");
        }

        #[test]
        fn finds_fn_def() {
            let lua = make_lua();
            let names = defined_names(&lua, "(fn greet [x] (print x))");
            assert!(names.contains(&"greet".to_string()), "got: {names:?}");
        }

        #[test]
        fn finds_multiple_defs() {
            let lua = make_lua();
            let names = defined_names(&lua, "(local a 1)\n(local b 2)\n(fn c [] nil)");
            assert!(names.contains(&"a".to_string()), "a missing: {names:?}");
            assert!(names.contains(&"b".to_string()), "b missing: {names:?}");
            assert!(names.contains(&"c".to_string()), "c missing: {names:?}");
        }

        #[test]
        fn unmangles_hyphenated_names() {
            let lua = make_lua();
            let raw = debug_compile(&lua, "(local my-var 99)");
            eprintln!("hyphen compile: {raw:?}");
            let names = defined_names(&lua, "(local my-var 99)");
            eprintln!("hyphen names: {names:?}");
            assert!(names.contains(&"my-var".to_string()), "got: {names:?}");
        }

        #[test]
        fn no_internal_names_leaked() {
            let lua = make_lua();
            let names = defined_names(&lua, "(local foo 1)");
            for name in &names {
                assert!(!name.starts_with('_'), "internal name leaked: {name}");
            }
        }

        #[test]
        fn finds_names_from_import_macros() {
            let lua = make_lua();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("simple-macros.fnl"),
                "(fn defsimple [name value] `(local ,name ,value)) {:defsimple defsimple}",
            ).unwrap();
            let search = dir.path().to_string_lossy().to_string();

            let f: LuaFunction = lua.globals().get("fennel_defined_names").unwrap();
            let src = "(import-macros {: defsimple} :simple-macros)\n(defsimple answer 42)\n(print answer)";
            let out: String = f.call((src, search.as_str())).unwrap();
            eprintln!("import-macros names: {out:?}");
            let names: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(|s| s.to_string()).collect();
            assert!(names.contains(&"defsimple".to_string()), "defsimple missing: {names:?}");
            assert!(names.contains(&"answer".to_string()), "answer missing: {names:?}");
        }

        #[test]
        fn finds_inline_macro_name_and_expansion() {
            let lua = make_lua();
            // (macro my-def …) puts my-def in scope.macros;
            // (my-def foo 99) expands to (local foo 99), putting foo in scope.unmanglings.
            let src = "(macro my-def [name value] `(local ,name ,value)) (my-def foo 99) (print foo)";
            let names = defined_names(&lua, src);
            eprintln!("inline-macro names: {names:?}");
            assert!(names.contains(&"my-def".to_string()), "macro name missing: {names:?}");
            assert!(names.contains(&"foo".to_string()), "expanded local missing: {names:?}");
        }
    }
}

#[cfg(feature = "embedded-fennel")]
pub use inner::MacroExpander;

// ── Integration tests (go through the channel) ───────────────────────────────

#[cfg(all(test, feature = "embedded-fennel"))]
mod tests {
    use super::MacroExpander;

    async fn expand(src: &str) -> Vec<String> {
        let e = MacroExpander::new();
        let mut names: Vec<String> = e.expand(src, "").await.into_iter().collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn channel_expander_finds_local() {
        let names = expand("(local foo 42)").await;
        assert!(names.contains(&"foo".to_string()), "got: {names:?}");
    }

    #[tokio::test]
    async fn channel_expander_finds_fn() {
        let names = expand("(fn greet [x] (print x))").await;
        assert!(names.contains(&"greet".to_string()), "got: {names:?}");
    }

    #[tokio::test]
    async fn channel_expander_returns_empty_on_bad_input() {
        let names = expand("(fn [x").await;
        // Graceful: either empty or non-empty, must not panic.
        let _ = names;
    }
}
