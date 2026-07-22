/// Built-in documentation for Lua standard library and Fennel special forms.

use std::collections::HashMap;

// ── BuiltinDoc ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BuiltinDoc {
    pub signature: String,
    pub doc: String,
    pub kind: BuiltinKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinKind {
    Function,
    Macro,
    SpecialForm,
    Value,
}

// ── Platform ──────────────────────────────────────────────────────────────────

/// The Lua runtime / flavour to target. Determines which globals and libraries
/// are recognised as known so the LSP does not emit spurious unknown-identifier
/// warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum Platform {
    /// Standard Lua 5.1. `unpack` is a global; no `utf8` / `bit32`.
    Lua51,
    /// Standard Lua 5.2. Adds `bit32`; `unpack` moved to `table.unpack`.
    Lua52,
    /// Standard Lua 5.3. Adds `utf8`; removes `bit32`; native bitwise ops.
    Lua53,
    /// Standard Lua 5.4 (default). Adds `warn`.
    #[default]
    Lua54,
    /// LuaJIT. Based on Lua 5.1 plus `ffi`, `jit`, and `bit`.
    LuaJIT,
    /// Luau (Roblox). Adds `task`, `script`, `game`, `workspace`.
    Luau,
}

// ── BuiltinSet ────────────────────────────────────────────────────────────────

/// The set of known built-in names for a particular platform, optionally
/// extended with library-specific definitions (e.g. LÖVE, Neovim, etc.).
///
/// ```
/// use fennel_ls::docs::{BuiltinSet, BuiltinDoc, BuiltinKind, Platform};
///
/// let set = BuiltinSet::for_platform(Platform::LuaJIT)
///     .with_extra([("love", BuiltinDoc {
///         signature: "love".into(),
///         doc: "LÖVE game framework global.".into(),
///         kind: BuiltinKind::Value,
///     })]);
///
/// assert!(set.get("love.graphics").is_some());
/// ```
pub struct BuiltinSet {
    map: HashMap<String, BuiltinDoc>,
}

impl BuiltinSet {
    /// Build the default builtin set for `platform`.
    pub fn for_platform(platform: Platform) -> Self {
        let mut map = build_common_map();
        match platform {
            Platform::Lua51 => {
                add_lua51_extras(&mut map);
            }
            Platform::Lua52 => {
                add_lua52_extras(&mut map);
            }
            Platform::Lua53 => {
                add_lua53_extras(&mut map);
            }
            Platform::Lua54 => {
                add_lua53_extras(&mut map);
                add_lua54_extras(&mut map);
            }
            Platform::LuaJIT => {
                add_lua51_extras(&mut map);
                add_luajit_extras(&mut map);
            }
            Platform::Luau => {
                add_luau_extras(&mut map);
            }
        }
        Self { map }
    }

    /// Extend this set with additional entries — e.g. a game-framework or
    /// editor API. Entries with the same name as existing ones overwrite them.
    #[allow(dead_code)]
    pub fn with_extra(mut self, entries: impl IntoIterator<Item = (impl Into<String>, BuiltinDoc)>) -> Self {
        for (name, doc) in entries {
            self.map.insert(name.into(), doc);
        }
        self
    }

    /// Look up `name` in the set. Supports multisym: `io.open` resolves via
    /// the root `io`, and `obj:method` resolves via `obj`.
    pub fn get(&self, name: &str) -> Option<&BuiltinDoc> {
        if let Some(doc) = self.map.get(name) {
            return Some(doc);
        }
        // Strip method/field suffix
        let root = name.split(['.', ':']).find(|s| !s.is_empty())?;
        if root != name {
            return self.map.get(root);
        }
        None
    }

    /// Return `true` if `name` (or its multisym root) is a known builtin.
    #[allow(dead_code)]
    pub fn is_known(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Iterate over all entries in insertion-stable order (actually HashMap
    /// order — callers should sort if they need stability).
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &BuiltinDoc)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }
}

// ── Default static set (Lua 5.4) ─────────────────────────────────────────────

/// A lazily-initialised `Lua54` builtin set, used by the backward-compat
/// free functions below.
pub fn default_set() -> &'static BuiltinSet {
    use std::sync::OnceLock;
    static SET: OnceLock<BuiltinSet> = OnceLock::new();
    SET.get_or_init(|| BuiltinSet::for_platform(Platform::Lua54))
}

/// Look up `name` in the default (Lua 5.4) builtin set.
#[allow(dead_code)]
pub fn get(name: &str) -> Option<&'static BuiltinDoc> {
    default_set().get(name)
}

// ── Map builders ─────────────────────────────────────────────────────────────

macro_rules! doc {
    ($m:expr, $name:expr, $sig:expr, $kind:ident, $doc:expr) => {
        $m.insert(
            $name.into(),
            BuiltinDoc {
                signature: $sig.into(),
                doc: $doc.into(),
                kind: BuiltinKind::$kind,
            },
        );
    };
}

/// Entries present in every supported Lua flavour.
fn build_common_map() -> HashMap<String, BuiltinDoc> {
    let mut m = HashMap::new();

    // ── Fennel special forms ─────────────────────────────────────────────────
    doc!(m, "fn",     "(fn name? [params...] body...)", SpecialForm,
        "Define a function. If `name` is given, binds it locally.");
    doc!(m, "lambda", "(lambda [params...] body...)", SpecialForm,
        "Define a function. Arguments are checked for nil at runtime.");
    doc!(m, "λ",      "(λ [params...] body...)", SpecialForm,
        "Alias for `lambda`.");
    doc!(m, "local",  "(local name value)", SpecialForm,
        "Introduce an immutable local binding.");
    doc!(m, "var",    "(var name value)", SpecialForm,
        "Introduce a mutable local binding (can be updated with `set`).");
    doc!(m, "global", "(global name value)", SpecialForm,
        "Introduce a global binding.");
    doc!(m, "set",    "(set name value)", SpecialForm,
        "Assign a new value to a `var` binding.");
    doc!(m, "tset",   "(tset table key value)", SpecialForm,
        "Set a field in a table: equivalent to `table[key] = value`.");
    doc!(m, "let",    "(let [name val ...] body...)", SpecialForm,
        "Sequential local bindings. Supports destructuring.");
    doc!(m, "do",     "(do body...)", SpecialForm,
        "Evaluate body forms in a new scope; return the last value.");
    doc!(m, "if",     "(if cond then else?)", SpecialForm,
        "Conditional. Multiple cond/then pairs are supported.");
    doc!(m, "when",   "(when cond body...)", SpecialForm,
        "Like `if` but without an else branch.");
    doc!(m, "unless", "(unless cond body...)", SpecialForm,
        "Evaluate body when cond is falsy.");
    doc!(m, "while",  "(while cond body...)", SpecialForm,
        "Evaluate body in a loop while cond is truthy.");
    doc!(m, "each",   "(each [key val iter-expr] body...)", SpecialForm,
        "Iterate with `each` (like Lua's generic for).");
    doc!(m, "for",    "(for [var start stop step?] body...)", SpecialForm,
        "Numeric for loop.");
    doc!(m, "match",  "(match value pattern body ...)", SpecialForm,
        "Pattern-match `value` against patterns.");
    doc!(m, "case",   "(case value pattern body ...)", SpecialForm,
        "Like `match` but patterns cannot rebind outer locals.");
    doc!(m, "require","(require :module)", SpecialForm,
        "Load and return a module.");
    doc!(m, "values", "(values ...)", SpecialForm,
        "Return multiple values.");
    doc!(m, "and",    "(and ...)", SpecialForm,
        "Boolean and (short-circuit); returns last truthy value or first falsy.");
    doc!(m, "or",     "(or ...)", SpecialForm,
        "Boolean or (short-circuit); returns first truthy value or last value.");
    doc!(m, "not",    "(not x)", SpecialForm,
        "Boolean negation.");
    doc!(m, ".",      "(. tbl key ...)", SpecialForm,
        "Table field access: `(. t :k)` → `t.k`.");
    doc!(m, "..",     "(.. s1 s2 ...)", SpecialForm,
        "String concatenation.");
    doc!(m, "#",      "(# t)", SpecialForm,
        "Length operator: `(# t)` → `#t`.");
    doc!(m, "macro",  "(macro name [params] body)", SpecialForm,
        "Define a compile-time macro.");
    doc!(m, "macros", "(macros {name fn ...})", SpecialForm,
        "Define multiple macros at once.");
    doc!(m, "import-macros", "(import-macros {name :local} :module)", SpecialForm,
        "Import macros from a module.");
    doc!(m, "include","(include :module)", SpecialForm,
        "Inline a module's compiled Lua source at compile time.");
    doc!(m, "lua",    "(lua code-string)", SpecialForm,
        "Emit raw Lua code.");
    doc!(m, "pick-values", "(pick-values n ...)", SpecialForm,
        "Limit multiple return values to exactly n.");
    doc!(m, "pick-args",   "(pick-args n f)", SpecialForm,
        "Wrap f to accept exactly n arguments.");
    doc!(m, "set-forcibly!", "(set-forcibly! name value)", SpecialForm,
        "Compiler-internal macro: forcibly set a binding (bypasses immutability).");
    doc!(m, "case-try",  "(case-try val pat body ... catch pat body)", SpecialForm,
        "Like `match-try` but patterns don't rebind locals.");
    doc!(m, "match-try", "(match-try val pat body ... catch pat body)", SpecialForm,
        "Chain pattern matches; jump to catch on mismatch.");
    doc!(m, "with-open", "(with-open [name (io.open ...)] body...)", SpecialForm,
        "Open a resource and close it automatically when the scope exits.");
    doc!(m, "collect",    "(collect [key val iter] body...)", SpecialForm,
        "Build a table by collecting key-value pairs from iteration.");
    doc!(m, "icollect",   "(icollect [val iter] body...)", SpecialForm,
        "Build a sequential table from iteration.");
    doc!(m, "accumulate", "(accumulate [acc init iter] body...)", SpecialForm,
        "Fold over iteration; like a left fold.");
    doc!(m, "faccumulate","(faccumulate [acc init i start stop step?] body...)", SpecialForm,
        "Numeric fold.");
    doc!(m, "fcollect",   "(fcollect [i start stop step? &into t?] body...)", SpecialForm,
        "Numeric collect: like `icollect` but with explicit start/stop/step.");
    doc!(m, "&into",  "&into",  SpecialForm, "Modifier: accumulate results into an existing table.");
    doc!(m, "&as",    "&as",    SpecialForm, "Modifier: bind the whole destructured value to a name.");
    doc!(m, "&until", "&until", SpecialForm, "Modifier: stop iteration early when condition is true.");
    doc!(m, "catch",  "catch",  SpecialForm, "Marker for catch clause in `case-try`/`match-try`.");
    doc!(m, "where",  "(where pattern guard)", SpecialForm, "Pattern guard in `match`/`case`.");
    doc!(m, "_",      "_", Value, "Discard pattern / ignored value.");

    // ── Fennel arithmetic / comparison operators ──────────────────────────────
    doc!(m, "+",   "(+ ...)",    SpecialForm, "Addition.");
    doc!(m, "-",   "(- a b?)",   SpecialForm, "Subtraction or negation.");
    doc!(m, "*",   "(* ...)",    SpecialForm, "Multiplication.");
    doc!(m, "/",   "(/ a b)",    SpecialForm, "Division.");
    doc!(m, "//",  "(// a b)",   SpecialForm, "Integer (floor) division.");
    doc!(m, "%",   "(% a b)",    SpecialForm, "Modulo.");
    doc!(m, "^",   "(^ a b)",    SpecialForm, "Exponentiation.");
    doc!(m, "<",   "(< a b ...)",  SpecialForm, "Less-than.");
    doc!(m, "<=",  "(<= a b ...)", SpecialForm, "Less-than-or-equal.");
    doc!(m, ">",   "(> a b ...)",  SpecialForm, "Greater-than.");
    doc!(m, ">=",  "(>= a b ...)", SpecialForm, "Greater-than-or-equal.");
    doc!(m, "=",   "(= a b ...)",  SpecialForm, "Equality.");
    doc!(m, "not=","(not= a b ...)",SpecialForm, "Inequality.");
    doc!(m, "~=",  "(~= a b)",  SpecialForm, "Inequality (Lua-style).");
    doc!(m, ":",   "(: obj method args...)", SpecialForm,
        "Method call: `(: obj :method args)` → `obj:method(args)`.");
    doc!(m, "?.",  "(?. t key ...)", SpecialForm,
        "Nil-safe table access: returns nil if any intermediate is nil.");
    doc!(m, "band",   "(band a b)", SpecialForm, "Bitwise AND.");
    doc!(m, "bor",    "(bor a b)",  SpecialForm, "Bitwise OR.");
    doc!(m, "bxor",   "(bxor a b)", SpecialForm, "Bitwise XOR.");
    doc!(m, "bnot",   "(bnot a)",   SpecialForm, "Bitwise NOT.");
    doc!(m, "lshift", "(lshift a n)", SpecialForm, "Left shift.");
    doc!(m, "rshift", "(rshift a n)", SpecialForm, "Right shift.");

    // ── Fennel threading macros ───────────────────────────────────────────────
    doc!(m, "->",   "(-> val form ...)",   Macro, "Thread `val` as first argument through each form.");
    doc!(m, "->>",  "(->> val form ...)",  Macro, "Thread `val` as last argument through each form.");
    doc!(m, "-?>",  "(-?> val form ...)",  Macro, "Nil-safe `->`: short-circuit on nil.");
    doc!(m, "-?>>", "(-?>> val form ...)", Macro, "Nil-safe `->>`.");
    doc!(m, "as->",    "(as-> val name form ...)", Macro, "Thread `val` through forms, binding the intermediate result to `name` in each form.");
    doc!(m, "doto", "(doto val form ...)", Macro, "Evaluate each form with `val` as first arg, then return `val`.");
    doc!(m, "partial", "(partial f args...)", Macro, "Partial application: return a fn with the given args pre-filled.");

    // ── Core Lua globals (common to all versions) ─────────────────────────────
    doc!(m, "print",       "(print ...)",           Function, "Print values to stdout, separated by tabs.");
    doc!(m, "tostring",    "(tostring x)",           Function, "Convert `x` to a string.");
    doc!(m, "tonumber",    "(tonumber x base?)",     Function, "Convert `x` to a number, optionally in `base`.");
    doc!(m, "type",        "(type x)",               Function, "Return the Lua type of `x` as a string.");
    doc!(m, "error",       "(error msg level?)",     Function, "Raise an error.");
    doc!(m, "assert",      "(assert v msg?)",        Function, "Assert `v` is truthy; raise `msg` otherwise.");
    doc!(m, "pcall",       "(pcall f ...)",          Function, "Call `f` in protected mode; returns `(true results...)` or `(false error)`.");
    doc!(m, "xpcall",      "(xpcall f handler ...)", Function, "Like `pcall` but calls `handler` on error.");
    doc!(m, "ipairs",      "(ipairs t)",             Function, "Iterator for sequential table values.");
    doc!(m, "pairs",       "(pairs t)",              Function, "Iterator for all table key-value pairs.");
    doc!(m, "next",        "(next t key?)",          Function, "Return the next key-value pair after `key` in `t`.");
    doc!(m, "select",      "(select index ...)",     Function, "Return all arguments after `index`, or their count.");
    doc!(m, "setmetatable","(setmetatable t mt)",    Function, "Set the metatable of `t` to `mt`.");
    doc!(m, "getmetatable","(getmetatable t)",       Function, "Return the metatable of `t`.");
    doc!(m, "rawget",      "(rawget t k)",           Function, "Get `t[k]` bypassing metamethods.");
    doc!(m, "rawset",      "(rawset t k v)",         Function, "Set `t[k] = v` bypassing metamethods.");
    doc!(m, "rawequal",    "(rawequal a b)",         Function, "Compare `a` and `b` without metamethods.");
    doc!(m, "rawlen",      "(rawlen v)",             Function, "Length of table or string without __len metamethod.");
    doc!(m, "require",     "(require modname)",      Function, "Load module `modname`.");
    doc!(m, "load",        "(load chunk ...)",       Function, "Load a Lua chunk.");
    doc!(m, "loadstring",  "(loadstring s name?)",   Function, "Load a string as a Lua chunk (Lua 5.1 compat).");
    doc!(m, "dofile",      "(dofile filename?)",     Function, "Load and execute a Lua file.");
    doc!(m, "loadfile",    "(loadfile filename? ...)",Function,"Load a Lua file without executing it.");
    doc!(m, "collectgarbage","(collectgarbage opt? arg?)", Function, "Control the garbage collector.");
    doc!(m, "length",      "(length t)", Function, "Return the length of `t` (like `#t`).");

    // ── Library roots (common) ────────────────────────────────────────────────
    doc!(m, "string",    "string",    Value, "Lua string library.");
    doc!(m, "table",     "table",     Value, "Lua table library.");
    doc!(m, "math",      "math",      Value, "Lua math library.");
    doc!(m, "io",        "io",        Value, "Lua I/O library.");
    doc!(m, "os",        "os",        Value, "Lua operating system library.");
    doc!(m, "coroutine", "coroutine", Value, "Lua coroutine library.");
    doc!(m, "debug",     "debug",     Value, "Lua debug library.");
    doc!(m, "package",   "package",   Value, "Lua package / module system.");

    // ── Universal globals ─────────────────────────────────────────────────────
    doc!(m, "_G",       "_G",       Value, "Global environment table.");
    doc!(m, "_VERSION", "_VERSION", Value, "Lua version string.");
    doc!(m, "arg",      "arg",      Value, "Command-line arguments (when run as a script).");
    doc!(m, "_ENV",     "_ENV",     Value, "The current environment table.");
    doc!(m, "fennel",   "fennel",   Value, "The Fennel compiler/runtime table.");
    doc!(m, "___repl___","___repl___",Value,"Fennel REPL state (internal).");

    m
}

fn add_lua51_extras(m: &mut HashMap<String, BuiltinDoc>) {
    // In Lua 5.1, `unpack` is a global (in 5.2+ it moved to `table.unpack`).
    doc!(m, "unpack", "(unpack t i? j?)", Function,
        "Unpack table elements as multiple return values. (Lua 5.1; use `table.unpack` in 5.2+.)");
}

fn add_lua52_extras(m: &mut HashMap<String, BuiltinDoc>) {
    doc!(m, "bit32", "bit32", Value, "Lua 5.2 bitwise operations library.");
}

fn add_lua53_extras(m: &mut HashMap<String, BuiltinDoc>) {
    doc!(m, "utf8", "utf8", Value, "Lua 5.3+ UTF-8 library.");
}

fn add_lua54_extras(m: &mut HashMap<String, BuiltinDoc>) {
    doc!(m, "warn", "(warn msg ...)", Function, "Emit a warning message (Lua 5.4+).");
}

fn add_luajit_extras(m: &mut HashMap<String, BuiltinDoc>) {
    doc!(m, "ffi", "ffi", Value, "LuaJIT FFI library for calling C functions.");
    doc!(m, "jit", "jit", Value, "LuaJIT JIT-compiler control.");
    doc!(m, "bit", "bit", Value, "LuaJIT bit-manipulation library.");
}

fn add_luau_extras(m: &mut HashMap<String, BuiltinDoc>) {
    doc!(m, "task",      "task",      Value, "Roblox task scheduler.");
    doc!(m, "script",    "script",    Value, "The Script instance containing this code.");
    doc!(m, "game",      "game",      Value, "The root DataModel instance.");
    doc!(m, "workspace", "workspace", Value, "The Workspace service.");
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Common entries present on all platforms ───────────────────────────────

    #[test]
    fn common_builtin_resolves() {
        for platform in [
            Platform::Lua51, Platform::Lua52, Platform::Lua53,
            Platform::Lua54, Platform::LuaJIT, Platform::Luau,
        ] {
            let set = BuiltinSet::for_platform(platform);
            assert!(set.get("print").is_some(),   "print missing on {platform:?}");
            assert!(set.get("pairs").is_some(),   "pairs missing on {platform:?}");
            assert!(set.get("tostring").is_some(),"tostring missing on {platform:?}");
            assert!(set.get("fn").is_some(),      "fn missing on {platform:?}");
            assert!(set.get("+").is_some(),       "+ missing on {platform:?}");
        }
    }

    #[test]
    fn unknown_name_returns_none() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        assert!(set.get("definitely_not_a_builtin_xyz").is_none());
    }

    // ── Multisym lookup ───────────────────────────────────────────────────────

    #[test]
    fn multisym_dot_resolves_via_root() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        // `io` is a known root; `io.open` should resolve via it
        assert!(set.get("io.open").is_some());
        assert!(set.get("string.format").is_some());
        assert!(set.get("math.max").is_some());
        assert!(set.get("table.insert").is_some());
    }

    #[test]
    fn multisym_colon_resolves_via_root() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        assert!(set.get("string:format").is_some());
    }

    #[test]
    fn unknown_multisym_returns_none() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        assert!(set.get("mylib.foo").is_none());
    }

    // ── Platform-specific entries ─────────────────────────────────────────────

    #[test]
    fn lua51_has_unpack_global() {
        assert!(BuiltinSet::for_platform(Platform::Lua51).get("unpack").is_some());
    }

    #[test]
    fn lua54_does_not_have_unpack_global() {
        // In Lua 5.4 `unpack` is `table.unpack` — bare `unpack` is a bug
        assert!(BuiltinSet::for_platform(Platform::Lua54).get("unpack").is_none());
    }

    #[test]
    fn lua54_has_warn() {
        assert!(BuiltinSet::for_platform(Platform::Lua54).get("warn").is_some());
    }

    #[test]
    fn lua51_does_not_have_warn() {
        assert!(BuiltinSet::for_platform(Platform::Lua51).get("warn").is_none());
    }

    #[test]
    fn luajit_has_ffi_jit_bit() {
        let set = BuiltinSet::for_platform(Platform::LuaJIT);
        assert!(set.get("ffi").is_some());
        assert!(set.get("jit").is_some());
        assert!(set.get("bit").is_some());
        // LuaJIT is based on Lua 5.1 so also has `unpack`
        assert!(set.get("unpack").is_some());
    }

    #[test]
    fn lua54_does_not_have_ffi() {
        assert!(BuiltinSet::for_platform(Platform::Lua54).get("ffi").is_none());
    }

    #[test]
    fn lua52_has_bit32() {
        assert!(BuiltinSet::for_platform(Platform::Lua52).get("bit32").is_some());
    }

    #[test]
    fn lua54_does_not_have_bit32() {
        assert!(BuiltinSet::for_platform(Platform::Lua54).get("bit32").is_none());
    }

    #[test]
    fn luau_has_roblox_globals() {
        let set = BuiltinSet::for_platform(Platform::Luau);
        assert!(set.get("task").is_some());
        assert!(set.get("script").is_some());
        assert!(set.get("game").is_some());
    }

    #[test]
    fn lua53_has_utf8() {
        let set = BuiltinSet::for_platform(Platform::Lua53);
        assert!(set.get("utf8").is_some());
    }

    // ── with_extra ────────────────────────────────────────────────────────────

    #[test]
    fn with_extra_adds_entry() {
        let set = BuiltinSet::for_platform(Platform::Lua54).with_extra([(
            "love",
            BuiltinDoc {
                signature: "love".into(),
                doc: "LÖVE game framework global.".into(),
                kind: BuiltinKind::Value,
            },
        )]);
        assert!(set.get("love").is_some());
    }

    #[test]
    fn with_extra_multisym_resolves_via_root() {
        let set = BuiltinSet::for_platform(Platform::Lua54).with_extra([(
            "love",
            BuiltinDoc { signature: "love".into(), doc: "LÖVE".into(), kind: BuiltinKind::Value },
        )]);
        // `love.graphics` should resolve via the `love` root
        assert!(set.get("love.graphics").is_some());
        assert!(set.get("love.audio").is_some());
    }

    #[test]
    fn with_extra_overrides_existing() {
        let custom_doc = BuiltinDoc {
            signature: "(print custom)".into(),
            doc: "Custom print override.".into(),
            kind: BuiltinKind::Function,
        };
        let set = BuiltinSet::for_platform(Platform::Lua54)
            .with_extra([("print", custom_doc.clone())]);
        assert_eq!(set.get("print").unwrap().doc, "Custom print override.");
    }

    // ── is_known ─────────────────────────────────────────────────────────────

    #[test]
    fn is_known_true_for_builtins() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        assert!(set.is_known("print"));
        assert!(set.is_known("io.open"));
    }

    #[test]
    fn is_known_false_for_unknowns() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        assert!(!set.is_known("my_custom_global"));
    }

    // ── iter ─────────────────────────────────────────────────────────────────

    #[test]
    fn iter_contains_all_entries() {
        let set = BuiltinSet::for_platform(Platform::Lua54);
        let names: Vec<&str> = set.iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"print"));
        assert!(names.contains(&"io"));
        assert!(names.contains(&"fn"));
    }

    // ── backward-compat free functions ────────────────────────────────────────

    #[test]
    fn free_get_resolves_common_builtin() {
        assert!(get("print").is_some());
        assert!(get("io.open").is_some());
        assert!(get("nonexistent_xyz").is_none());
    }
}
