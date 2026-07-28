/// Macro analysis hooks: user-supplied Fennel functions that guide the language
/// server's analysis of custom macro calls.
///
/// Hooks are configured in `.lsp.fnl` under `:macro-hooks`:
///
///   {:macro-hooks
///     {:my-macro (fn [call] ...)}}
///
/// The hook receives the macro call as a Lua table (`call`) with:
///   - `call.children`  — 1-indexed array of child nodes (starting with the macro name)
///   - `call.span`      — `{start, end, line, col, end_line, end_col}`
///
/// Each child node carries:
///   - `kind`      — "sym", "keyword", "str", "num", "bool", "nil", "vararg",
///                   "list", "table", "seq", or "other" for reader-macro forms
///   - `value`     — string value for leaf nodes (sym, keyword, str, num, bool)
///   - `children`  — array of child nodes for compound forms (list, table, seq)
///   - `span`      — same fields as call.span
///
/// The hook returns an array of instruction tables processed in order:
///
///   {:type "bind"   :name "MyClass" :span {start end line col end_line end_col}}
///     — introduce a named binding at the given source span
///
///   {:type "analyze"    :index 3}
///     — resolve references inside call.children[3] as normal Fennel code
///
///   {:type "analyze-fn" :index 3}
///     — analyze call.children[3] as a `(fn name? [params] body...)` definition;
///       this gives hover, go-to-def, completion, and arity-checking for free
///
///   {:type "scope-open"  :span {...}}
///     — push a new lexical scope covering the given span
///
///   {:type "scope-close"}
///     — pop the most recently opened scope
///
///   {:type "sub-form-completions" :index 3 :completions ["extends" "fn" "signal"]}
///     — when the cursor is inside call.children[3], offer these fixed completions
///
/// Children NOT mentioned by any instruction are silently skipped (no analysis,
/// no unknown-identifier warnings).
///
/// Example — defnode hook:
///
///   (fn [call]
///     (let [result []
///           name-node (. call.children 2)]
///       (table.insert result {:type "bind" :name name-node.value :span name-node.span})
///       (for [i 3 (length call.children)]
///         (let [form (. call.children i)
///               head (. form.children 1)]
///           (when (= head.value "fn")
///             (table.insert result {:type "analyze-fn" :index i}))))
///       result))

use std::collections::HashMap;
use std::sync::Arc;
use dashmap::DashMap;
use mlua::prelude::*;
use tokio::sync::{mpsc, oneshot};

use crate::lexer::Span;
use crate::parser::{AstNode, Form};

// ── AST wire format ───────────────────────────────────────────────────────────

/// A Lua-friendly view of an AST node for passing to hook functions.
#[derive(Debug, Clone)]
pub struct SerialNode {
    pub kind: &'static str,
    pub value: Option<String>,
    pub children: Vec<SerialNode>,
    pub span: Span,
}

impl SerialNode {
    pub fn from_ast(node: &AstNode) -> Self {
        match &node.node {
            Form::List(ch) => Self {
                kind: "list",
                value: None,
                children: ch.iter().map(Self::from_ast).collect(),
                span: node.span.clone(),
            },
            Form::Table(ch) => Self {
                kind: "table",
                value: None,
                children: ch.iter().map(Self::from_ast).collect(),
                span: node.span.clone(),
            },
            Form::Sequence(ch) => Self {
                kind: "seq",
                value: None,
                children: ch.iter().map(Self::from_ast).collect(),
                span: node.span.clone(),
            },
            Form::Symbol(s) => Self::leaf("sym", Some(s.clone()), node),
            Form::Keyword(s) => Self::leaf("keyword", Some(s.clone()), node),
            Form::Str(s) => Self::leaf("str", Some(s.clone()), node),
            Form::Number(n) => Self::leaf("num", Some(format!("{n}")), node),
            Form::Bool(b) => Self::leaf("bool", Some(b.to_string()), node),
            Form::Nil => Self::leaf("nil", None, node),
            Form::Varargs => Self::leaf("vararg", None, node),
            _ => Self::leaf("other", None, node),
        }
    }

    fn leaf(kind: &'static str, value: Option<String>, node: &AstNode) -> Self {
        Self { kind, value, children: vec![], span: node.span.clone() }
    }

    fn to_lua_table<'lua>(&self, lua: &'lua Lua) -> LuaResult<LuaTable<'lua>> {
        let t = lua.create_table()?;
        t.set("kind", self.kind)?;
        if let Some(v) = &self.value {
            t.set("value", v.as_str())?;
        }
        let s = lua.create_table()?;
        s.set("start", self.span.start)?;
        s.set("end", self.span.end)?;
        s.set("line", self.span.line)?;
        s.set("col", self.span.col)?;
        s.set("end_line", self.span.end_line)?;
        s.set("end_col", self.span.end_col)?;
        t.set("span", s)?;
        let ch = lua.create_table()?;
        for (i, child) in self.children.iter().enumerate() {
            ch.set(i + 1, child.to_lua_table(lua)?)?;
        }
        t.set("children", ch)?;
        Ok(t)
    }
}

// ── Instructions ──────────────────────────────────────────────────────────────

/// An operation returned by a hook function describing how to handle one part
/// of a macro call.
#[derive(Debug, Clone)]
pub enum Instruction {
    /// Introduce a named binding at the given source span.
    Bind { name: String, span: Span },
    /// Analyze call.children[index] as normal Fennel code (1-based).
    Analyze { index: usize },
    /// Analyze call.children[index] as a `(fn name? [params] body...)` form (1-based).
    AnalyzeFn { index: usize },
    /// Open a new lexical scope spanning `span`.
    ScopeOpen { span: Span },
    /// Close the most recently opened scope.
    ScopeClose,
    /// When the cursor is inside call.children[index], offer these completions (1-based).
    SubFormCompletions { index: usize, completions: Vec<String> },
}

impl Instruction {
    fn from_lua_table(t: &LuaTable) -> LuaResult<Self> {
        let kind: String = t.get("type")?;
        match kind.as_str() {
            "bind" => Ok(Self::Bind {
                name: t.get("name")?,
                span: span_from_lua(&t.get::<_, LuaTable>("span")?)?
            }),
            "analyze" => Ok(Self::Analyze { index: t.get("index")? }),
            "analyze-fn" => Ok(Self::AnalyzeFn { index: t.get("index")? }),
            "scope-open" => Ok(Self::ScopeOpen {
                span: span_from_lua(&t.get::<_, LuaTable>("span")?)?
            }),
            "scope-close" => Ok(Self::ScopeClose),
            "sub-form-completions" => {
                let comps: LuaTable = t.get("completions")?;
                let completions = comps
                    .sequence_values::<String>()
                    .filter_map(|v| v.ok())
                    .collect();
                Ok(Self::SubFormCompletions { index: t.get("index")?, completions })
            }
            other => Err(LuaError::RuntimeError(
                format!("unknown hook instruction type: {other}")
            )),
        }
    }
}

fn span_from_lua(t: &LuaTable) -> LuaResult<Span> {
    Ok(Span {
        start: t.get("start")?,
        end: t.get("end")?,
        line: t.get("line")?,
        col: t.get("col")?,
        end_line: t.get("end_line").unwrap_or(0),
        end_col: t.get("end_col").unwrap_or(0),
    })
}

// ── Hook runner ───────────────────────────────────────────────────────────────

enum HookMessage {
    SetSource { lsp_fnl_src: String, search_path: String },
    RunHook {
        source_module: String,
        macro_name: String,
        call_node: SerialNode,
        /// `None` = no hook registered; `Some(vec![])` = hook returned nothing.
        respond: oneshot::Sender<Option<Vec<Instruction>>>,
    },
}

const FENNEL_SRC: &str = include_str!("../vendor/fennel.lua");

const HOOK_HELPER: &str = r#"
-- _hooks layout after register_hooks:
--   _hooks["module.path"]["macro_name"] = fn   -- from nested {:module {:name fn}}
--   _hooks[""]["macro_name"]            = fn   -- from flat {:macro_name fn} (any module)
local _hooks = {}

function register_hooks(src, search_path)
    local orig_path = fennel.path
    local orig_macro_path = fennel["macro-path"]
    if search_path and search_path ~= "" then
        local prefix = search_path .. "/?.fnl;" .. search_path .. "/?/init.fnl;"
        fennel.path = prefix .. orig_path
        fennel["macro-path"] = prefix .. orig_macro_path
    end
    local ok, config = pcall(fennel.eval, src, {allowedGlobals = false})
    fennel.path = orig_path
    fennel["macro-path"] = orig_macro_path
    if not ok then return end
    if type(config) ~= "table" then return end
    local macro_hooks = config["macro-hooks"]
    if type(macro_hooks) ~= "table" then return end
    _hooks = {}
    for key, val in pairs(macro_hooks) do
        if type(val) == "function" then
            -- Flat syntax: {:macro-name fn}  →  matches any module
            if not _hooks[""] then _hooks[""] = {} end
            _hooks[""][key] = val
        elseif type(val) == "table" then
            -- Nested syntax: {"module.path" {:macro-name fn}}
            _hooks[key] = {}
            for name, fn_val in pairs(val) do
                if type(fn_val) == "function" then
                    _hooks[key][name] = fn_val
                end
            end
        end
    end
end

-- source_module is "" for inline (macro ...) definitions.
-- Returns nil when no hook is registered (lets Rust distinguish no-hook from empty instructions).
function run_hook(source_module, macro_name, call_node)
    local hook
    -- Module-specific hook takes priority
    if source_module ~= "" and _hooks[source_module] then
        hook = _hooks[source_module][macro_name]
    end
    -- Fall back to flat (any-module) hook
    if not hook and _hooks[""] then
        hook = _hooks[""][macro_name]
    end
    if not hook then return nil end
    local ok, result = pcall(hook, call_node)
    if not ok then return nil end
    if type(result) ~= "table" then return nil end
    return result
end
"#;

/// Results cached for a specific file version.
/// Maps call_span_start → list of instructions for that macro call.
pub type VersionedResults = (i32, HashMap<u32, Vec<Instruction>>);

/// Handle to the background hook-runner thread.
///
/// Cheap to clone — wraps a channel sender and a shared cache.
#[derive(Clone)]
pub struct HookRunner {
    tx: mpsc::Sender<HookMessage>,
    /// Per-file cache: URI string → (version, call_span_start → instructions).
    pub cache: Arc<DashMap<String, VersionedResults>>,
}

impl HookRunner {
    pub fn new() -> Self {
        let (tx, mut rx) = mpsc::channel::<HookMessage>(64);
        let cache: Arc<DashMap<String, VersionedResults>> = Arc::new(DashMap::new());

        std::thread::Builder::new()
            .name("fennel-hooks".into())
            .spawn(move || {
                // SAFETY: same rationale as expander.rs — Fennel uses debug.traceback.
                let lua = unsafe { Lua::unsafe_new() };
                let fennel_mod: LuaTable = match lua.load(FENNEL_SRC).set_name("fennel.lua").eval() {
                    Ok(t) => t,
                    Err(e) => { log::error!("hooks: failed to load Fennel: {e}"); return; }
                };
                if let Err(e) = lua.globals().set("fennel", fennel_mod) {
                    log::error!("hooks: failed to set fennel global: {e}"); return;
                }
                if let Err(e) = lua.load(HOOK_HELPER).exec() {
                    log::error!("hooks: failed to load helper: {e}"); return;
                }
                let register_fn: LuaFunction = match lua.globals().get("register_hooks") {
                    Ok(f) => f,
                    Err(e) => { log::error!("hooks: get register_hooks: {e}"); return; }
                };
                let run_fn: LuaFunction = match lua.globals().get("run_hook") {
                    Ok(f) => f,
                    Err(e) => { log::error!("hooks: get run_hook: {e}"); return; }
                };

                while let Some(msg) = rx.blocking_recv() {
                    match msg {
                        HookMessage::SetSource { lsp_fnl_src, search_path } => {
                            if let Err(e) = register_fn.call::<_, ()>((lsp_fnl_src, search_path)) {
                                log::warn!("hooks: register_hooks failed: {e}");
                            }
                        }
                        HookMessage::RunHook { source_module, macro_name, call_node, respond } => {
                            let instructions = call_hook(&lua, &run_fn, &source_module, &macro_name, &call_node);
                            let _ = respond.send(instructions);
                        }
                    }
                }
            })
            .expect("spawn hooks thread");

        Self { tx, cache }
    }

    /// Register hooks from `.lsp.fnl` source. Fire-and-forget; safe to call from
    /// synchronous context since the channel has ample capacity.
    pub fn try_set_source(&self, lsp_fnl_src: String, search_path: String) {
        let _ = self.tx.try_send(HookMessage::SetSource { lsp_fnl_src, search_path });
    }

    /// Run the hook for `(source_module, macro_name)` and return its instructions.
    ///
    /// Returns `None` when no hook is registered for that macro (useful for
    /// emitting "unhooked macro" warnings). Returns `Some(vec![])` when a hook
    /// exists but produced no instructions.
    ///
    /// `source_module` should be `""` for inline `(macro ...)` definitions.
    pub async fn run_hook(
        &self,
        source_module: &str,
        macro_name: &str,
        call_node: SerialNode,
    ) -> Option<Vec<Instruction>> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(HookMessage::RunHook {
            source_module: source_module.to_owned(),
            macro_name: macro_name.to_owned(),
            call_node,
            respond: tx,
        }).await.is_err() {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    /// Look up cached hook results for `(uri, version)`.
    /// Returns `None` if the cache is empty or the stored version differs.
    pub fn cached_results(&self, uri: &str, version: i32) -> Option<HashMap<u32, Vec<Instruction>>> {
        let entry = self.cache.get(uri)?;
        if entry.0 != version {
            return None;
        }
        Some(entry.1.clone())
    }

    /// Store hook results for `(uri, version)`, replacing any previous entry.
    pub fn store_results(&self, uri: String, version: i32, results: HashMap<u32, Vec<Instruction>>) {
        self.cache.insert(uri, (version, results));
    }
}

fn call_hook(
    lua: &Lua,
    run_fn: &LuaFunction,
    source_module: &str,
    macro_name: &str,
    call_node: &SerialNode,
) -> Option<Vec<Instruction>> {
    let call_table = match call_node.to_lua_table(lua) {
        Ok(t) => t,
        Err(e) => {
            log::debug!("hooks: serialize node for {macro_name}: {e}");
            return None;
        }
    };
    let result: Option<LuaTable> = match run_fn.call((source_module, macro_name, call_table)) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("hooks: run_hook({macro_name}) error: {e}");
            return None;
        }
    };
    // None from Lua means no hook registered.
    let tbl = result?;
    let instructions = tbl.sequence_values::<LuaTable>()
        .filter_map(|entry| {
            let t = entry.ok()?;
            match Instruction::from_lua_table(&t) {
                Ok(instr) => Some(instr),
                Err(e) => { log::debug!("hooks: bad instruction: {e}"); None }
            }
        })
        .collect();
    Some(instructions)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lua() -> Lua {
        // SAFETY: same rationale as expander.rs
        let lua = unsafe { Lua::unsafe_new() };
        let fennel_mod: LuaTable = lua.load(FENNEL_SRC).set_name("fennel.lua").eval().unwrap();
        lua.globals().set("fennel", fennel_mod).unwrap();
        lua.load(HOOK_HELPER).exec().unwrap();
        lua
    }

    fn register(lua: &Lua, lsp_fnl_src: &str) {
        let f: LuaFunction = lua.globals().get("register_hooks").unwrap();
        f.call::<_, ()>((lsp_fnl_src, "")).unwrap();
    }

    fn run(lua: &Lua, module: &str, name: &str, call: &SerialNode) -> Option<Vec<Instruction>> {
        let run_fn: LuaFunction = lua.globals().get("run_hook").unwrap();
        call_hook(lua, &run_fn, module, name, call)
    }

    fn dummy_call() -> SerialNode {
        let span = Span { start: 0, end: 1, line: 0, col: 0, end_line: 0, end_col: 1 };
        SerialNode {
            kind: "list",
            value: None,
            children: vec![
                SerialNode { kind: "sym", value: Some("my-macro".into()), children: vec![], span: span.clone() },
            ],
            span,
        }
    }

    // ── register_hooks / run_hook ─────────────────────────────────────────────

    #[test]
    fn no_hook_returns_none() {
        let lua = make_lua();
        register(&lua, "{}");
        assert!(run(&lua, "", "nonexistent", &dummy_call()).is_none());
    }

    #[test]
    fn flat_hook_matches_any_module() {
        let lua = make_lua();
        register(&lua, r#"
            {:macro-hooks {:my-macro (fn [call] [{:type "bind" :name "x"
                                                  :span {:start 0 :end 1 :line 0 :col 0 :end_line 0 :end_col 1}}])}}
        "#);
        // Flat hooks match even when a source_module is supplied
        let result = run(&lua, "some.module", "my-macro", &dummy_call());
        assert!(result.is_some(), "flat hook should match any module");
        let instrs = result.unwrap();
        assert_eq!(instrs.len(), 1);
        assert!(matches!(&instrs[0], Instruction::Bind { name, .. } if name == "x"));
    }

    #[test]
    fn module_hook_takes_priority_over_flat() {
        let lua = make_lua();
        // Flat hook {:my-macro fn} + module-specific hook under "lib.macros".
        // When called with module "lib.macros", the module-specific hook wins.
        register(&lua, r#"
            {:macro-hooks
              {:my-macro (fn [_] [{:type "scope-close"}])
               "lib.macros" {:my-macro (fn [_] [{:type "scope-open"
                                                  :span {:start 0 :end 1 :line 0 :col 0 :end_line 0 :end_col 1}}])}}}
        "#);
        let result = run(&lua, "lib.macros", "my-macro", &dummy_call()).unwrap();
        assert!(matches!(&result[0], Instruction::ScopeOpen { .. }),
            "module hook should win; got {:?}", result);
    }

    #[test]
    fn module_hook_falls_back_to_flat_for_other_modules() {
        let lua = make_lua();
        register(&lua, r#"
            {:macro-hooks
              {:my-macro (fn [_] [{:type "scope-close"}])
               "specific.lib" {:my-macro (fn [_] [{:type "scope-open"
                                                    :span {:start 0 :end 1 :line 0 :col 0 :end_line 0 :end_col 1}}])}}}
        "#);
        let result = run(&lua, "other.lib", "my-macro", &dummy_call()).unwrap();
        assert!(matches!(&result[0], Instruction::ScopeClose),
            "should fall back to flat hook for other.lib; got {:?}", result);
    }

    #[test]
    fn hook_returning_empty_table_is_some_not_none() {
        let lua = make_lua();
        register(&lua, "{:macro-hooks {:empty-hook (fn [_] [])}}");
        let result = run(&lua, "", "empty-hook", &dummy_call());
        assert!(result.is_some(), "hook that returns [] should be Some, not None");
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn hook_error_returns_none() {
        let lua = make_lua();
        register(&lua, "{:macro-hooks {:bad (fn [_] (error \"oops\"))}}");
        let result = run(&lua, "", "bad", &dummy_call());
        assert!(result.is_none(), "erroring hook should return None");
    }

    #[test]
    fn register_hooks_clears_old_hooks_on_reload() {
        let lua = make_lua();
        register(&lua, "{:macro-hooks {:old-macro (fn [_] [])}}");
        register(&lua, "{:macro-hooks {:new-macro (fn [_] [])}}");
        assert!(run(&lua, "", "old-macro", &dummy_call()).is_none(), "old hook should be gone");
        assert!(run(&lua, "", "new-macro", &dummy_call()).is_some(), "new hook should exist");
    }

    // ── Instruction round-trip ────────────────────────────────────────────────

    #[test]
    fn analyze_instruction_round_trips() {
        let lua = make_lua();
        register(&lua, "{:macro-hooks {:m (fn [_] [{:type \"analyze\" :index 2}])}}");
        let result = run(&lua, "", "m", &dummy_call()).unwrap();
        assert!(matches!(result[0], Instruction::Analyze { index: 2 }));
    }

    #[test]
    fn analyze_fn_instruction_round_trips() {
        let lua = make_lua();
        register(&lua, "{:macro-hooks {:m (fn [_] [{:type \"analyze-fn\" :index 3}])}}");
        let result = run(&lua, "", "m", &dummy_call()).unwrap();
        assert!(matches!(result[0], Instruction::AnalyzeFn { index: 3 }));
    }

    #[test]
    fn sub_form_completions_round_trips() {
        let lua = make_lua();
        register(&lua, r#"
            {:macro-hooks {:m (fn [_] [{:type "sub-form-completions"
                                        :index 2
                                        :completions ["extends" "fn" "signal"]}])}}
        "#);
        let result = run(&lua, "", "m", &dummy_call()).unwrap();
        match &result[0] {
            Instruction::SubFormCompletions { index: 2, completions } => {
                assert_eq!(completions, &["extends", "fn", "signal"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── SerialNode construction ───────────────────────────────────────────────

    #[test]
    fn serial_node_from_list_ast() {
        use crate::parser::{Form, Spanned};
        let span = Span { start: 0, end: 10, line: 0, col: 0, end_line: 0, end_col: 10 };
        let node = Spanned {
            node: Form::List(vec![
                Spanned { node: Form::Symbol("foo".into()), span: span.clone() },
                Spanned { node: Form::Number(42.0), span: span.clone() },
            ]),
            span: span.clone(),
        };
        let sn = SerialNode::from_ast(&node);
        assert_eq!(sn.kind, "list");
        assert_eq!(sn.children.len(), 2);
        assert_eq!(sn.children[0].kind, "sym");
        assert_eq!(sn.children[0].value.as_deref(), Some("foo"));
        assert_eq!(sn.children[1].kind, "num");
    }

    // ── HookRunner cache ─────────────────────────────────────────────────────

    #[test]
    fn cache_miss_when_version_differs() {
        let runner = HookRunner::new();
        runner.store_results("file:///a.fnl".into(), 1, HashMap::new());
        assert!(runner.cached_results("file:///a.fnl", 2).is_none(),
            "different version should be a cache miss");
    }

    #[test]
    fn cache_hit_when_version_matches() {
        let runner = HookRunner::new();
        let mut map = HashMap::new();
        map.insert(42u32, vec![Instruction::ScopeClose]);
        runner.store_results("file:///a.fnl".into(), 3, map);
        let cached = runner.cached_results("file:///a.fnl", 3).unwrap();
        assert!(cached.contains_key(&42));
    }
}
