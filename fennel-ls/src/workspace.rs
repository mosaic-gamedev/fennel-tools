/// File state management. Each open file is parsed and analyzed on every change.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use dashmap::DashMap;
use tower_lsp::lsp_types::{Location, Position, Range, Url};

use crate::analyzer::{AnalysisResult, DefinitionInfo};
use crate::docs::{BuiltinSet, Platform};
use crate::lexer::Span;
use crate::parser::{AstNode, ParseError};

/// A cross-file reference to a definition in another file.
#[derive(Debug, Clone)]
pub struct CrossFileRef {
    pub uri: Url,
    pub text: String,
    pub span: Span,
    /// Full multisym as it appears in the referencing file (e.g. `"utils.greet"`).
    pub sym_name: String,
}

/// Describes the origin of an analyzed module's text.
#[derive(Debug)]
pub enum ModuleSource {
    /// Native Fennel file — spans refer directly to the Fennel source.
    Fennel,
    /// Transpiled from a Lua file.
    ///
    /// `source_map[i]` gives the Lua source line (1-indexed) for Fennel output
    /// line `i + 1`. Used to remap definition spans back to their original Lua
    /// positions when producing LSP Locations.
    Lua { source_map: Vec<u32> },
}

/// Top-level exports from a required module file.
#[derive(Debug)]
pub struct ModuleExports {
    pub uri: Url,
    /// The Fennel text that was analyzed (either native source or transpiled from Lua).
    /// Byte offsets in `defs` refer to positions within this string.
    pub text: String,
    /// Top-level definitions, keyed by name.
    pub defs: HashMap<String, DefinitionInfo>,
    /// Where this module's text came from; drives position remapping in `location_for_def`.
    pub source: ModuleSource,
}

impl ModuleExports {
    /// Build an LSP Location pointing to `def` inside this module.
    ///
    /// For Fennel modules, byte span → LSP range directly.
    /// For Lua modules, the byte span is in the generated Fennel; we remap
    /// via the source map to the original Lua line before returning the Location.
    pub fn location_for_def(&self, def: &DefinitionInfo) -> Location {
        match &self.source {
            ModuleSource::Fennel => Location {
                uri: self.uri.clone(),
                range: crate::text::span_to_range(&self.text, &def.span),
            },
            ModuleSource::Lua { source_map } => {
                let fennel_line = newlines_before(&self.text, def.span.start as usize);
                let lua_line = source_map.get(fennel_line).copied().unwrap_or(1);
                let lsp_line = lua_line.saturating_sub(1);
                Location {
                    uri: self.uri.clone(),
                    range: Range {
                        start: Position { line: lsp_line, character: 0 },
                        end: Position { line: lsp_line, character: 0 },
                    },
                }
            }
        }
    }
}

/// Count the number of newlines before `byte` in `text` (= 0-indexed line of that byte).
fn newlines_before(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].bytes().filter(|&b| b == b'\n').count()
}

#[derive(Debug)]
pub struct AnalyzedFile {
    pub uri: Url,
    pub text: String,
    #[allow(dead_code)]
    pub version: i32,
    #[allow(dead_code)]
    pub ast: Vec<AstNode>,
    pub parse_errors: Vec<ParseError>,
    pub analysis: AnalysisResult,
    /// Resolved require bindings: local binding name → module exports.
    /// Populated from `(local name (require :mod))` forms when workspace root is set.
    pub modules: HashMap<String, Arc<ModuleExports>>,
    /// Names introduced by macro expansion (e.g. via `import-macros`).
    /// Populated asynchronously after the initial analysis pass.
    pub macro_globals: HashSet<String>,
    /// Macro call sites that have no hook defined, for optional diagnostics.
    /// Populated by the hook runner pass; empty when warn-unhooked-macros is off.
    pub unhooked_macros: Vec<(crate::lexer::Span, String)>,
}

impl AnalyzedFile {
    /// Find the list node (macro call) whose span starts at `byte`.
    /// Used to build a `SerialNode` for hook execution.
    pub fn find_list_at(&self, byte: u32) -> Option<&AstNode> {
        fn search(node: &AstNode, target: u32) -> Option<&AstNode> {
            if node.span.start == target {
                if let crate::parser::Form::List(_) = &node.node {
                    return Some(node);
                }
            }
            match &node.node {
                crate::parser::Form::List(ch)
                | crate::parser::Form::Table(ch)
                | crate::parser::Form::Sequence(ch) => {
                    for child in ch {
                        if let Some(found) = search(child, target) {
                            return Some(found);
                        }
                    }
                }
                crate::parser::Form::Quote(inner)
                | crate::parser::Form::Quasiquote(inner)
                | crate::parser::Form::Unquote(inner)
                | crate::parser::Form::UnquoteSplice(inner)
                | crate::parser::Form::HashFn(inner) => {
                    return search(inner, target);
                }
                _ => {}
            }
            None
        }
        for top in &self.ast {
            if let Some(found) = search(top, byte) {
                return Some(found);
            }
        }
        None
    }
}

#[derive(Clone)]
pub struct Workspace {
    files: Arc<DashMap<String, AnalyzedFile>>,
    // OnceLock so configure_platform() can be called from &self in initialize().
    builtins: Arc<OnceLock<Arc<BuiltinSet>>>,
    /// Cache of analyzed module files (required but not open).
    /// Keyed by absolute filesystem path; bounded by number of unique source files.
    require_cache: Arc<DashMap<std::path::PathBuf, Arc<ModuleExports>>>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            files: Arc::default(),
            builtins: Arc::new(OnceLock::new()),
            require_cache: Arc::default(),
        }
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the builtin set based on a platform selection. Must be called before
    /// any `builtins()` access; silently ignored if already initialised.
    pub fn configure_platform(&self, platform: Platform) {
        let _ = self.builtins.set(Arc::new(BuiltinSet::for_platform(platform)));
    }

    /// Replace the builtin set (builder style, for tests or pre-open config).
    #[allow(dead_code)]
    pub fn with_builtins(mut self, builtins: BuiltinSet) -> Self {
        // Replace the inner OnceLock so we can set a fresh value even if the
        // default has already been initialized.
        self.builtins = Arc::new(OnceLock::new());
        let _ = self.builtins.set(Arc::new(builtins));
        self
    }

    /// Access the active builtin set (for hover, completion, and diagnostics).
    pub fn builtins(&self) -> &BuiltinSet {
        self.builtins
            .get_or_init(|| Arc::new(BuiltinSet::for_platform(Platform::Lua54)))
            .as_ref()
    }

    /// Parse, analyze, and optionally resolve require bindings for a file.
    ///
    /// `workspace_root` is used to resolve `(require :mod)` to `.fnl` paths.
    /// Pass `None` (e.g. in tests) to skip cross-file resolution.
    ///
    /// `hook_results` is an optional map of macro call span → hook instructions
    /// from a previous hook-runner pass. Pass an empty map on the first pass.
    pub fn update(
        &self,
        uri: Url,
        text: String,
        version: i32,
        workspace_root: Option<&std::path::Path>,
        hook_results: &HashMap<u32, Vec<crate::hooks::Instruction>>,
    ) {
        // Invalidate the require_cache entry for this file so callers get
        // fresh exports after edits.
        if let Ok(path) = uri.to_file_path() {
            self.require_cache.remove(&path);
        }

        let (ast, parse_errors) = crate::parser::Parser::parse(&text);
        let analysis = crate::analyzer::analyze_with_hooks(&ast, hook_results);

        // Resolve require bindings → module exports
        let mut modules: HashMap<String, Arc<ModuleExports>> = HashMap::new();
        if let Some(root) = workspace_root {
            for (binding, module_name) in &analysis.module_bindings {
                if let Some(path) = resolve_require_path(module_name, root) {
                    if let Some(exports) = self.load_module(&path) {
                        modules.insert(binding.clone(), exports);
                    }
                }
            }
        }

        self.files.insert(
            uri.to_string(),
            AnalyzedFile {
                uri,
                text,
                version,
                ast,
                parse_errors,
                analysis,
                modules,
                macro_globals: HashSet::new(),
                unhooked_macros: Vec::new(),
            },
        );
    }

    pub fn remove(&self, uri: &Url) {
        self.files.remove(&uri.to_string());
    }

    /// Remove a file from the require cache so the next require of it re-reads disk.
    pub fn invalidate_require_cache(&self, path: &std::path::Path) {
        self.require_cache.remove(path);
    }

    /// Merge macro-expansion results into the file's scope.
    pub fn set_macro_globals(&self, uri: &Url, names: HashSet<String>) {
        if let Some(mut entry) = self.files.get_mut(&uri.to_string()) {
            entry.macro_globals = names;
        }
    }

    /// Store macro call sites that have no hook defined (for optional diagnostics).
    pub fn set_unhooked_macros(&self, uri: &Url, spans: Vec<(crate::lexer::Span, String)>) {
        if let Some(mut entry) = self.files.get_mut(&uri.to_string()) {
            entry.unhooked_macros = spans;
        }
    }

    /// Run `f` with a reference to the file, returning `None` if not found.
    pub fn with_file<F, R>(&self, uri: &Url, f: F) -> Option<R>
    where
        F: FnOnce(&AnalyzedFile) -> R,
    {
        let entry = self.files.get(&uri.to_string())?;
        Some(f(&*entry))
    }

    /// Returns the URIs of all currently open (tracked) files.
    pub fn all_open_uris(&self) -> Vec<Url> {
        self.files.iter().map(|e| e.value().uri.clone()).collect()
    }

    /// Search every open file and the require cache for definitions whose names
    /// contain `query` (case-insensitive). An empty query returns everything.
    /// Returns `(uri, file_text, def)` triples — callers convert spans to ranges.
    pub fn all_defs(&self, query: &str) -> Vec<(Url, String, DefinitionInfo)> {
        let q = query.to_lowercase();
        let mut out = Vec::new();

        for entry in self.files.iter() {
            let f = entry.value();
            for def in f.analysis.defs.values() {
                if q.is_empty() || def.name.to_lowercase().contains(&q) {
                    out.push((f.uri.clone(), f.text.clone(), def.clone()));
                }
            }
        }

        // Also search module files that are required but not currently open.
        for entry in self.require_cache.iter() {
            let exports = entry.value();
            for def in exports.defs.values() {
                if q.is_empty() || def.name.to_lowercase().contains(&q) {
                    out.push((exports.uri.clone(), exports.text.clone(), def.clone()));
                }
            }
        }

        out
    }

    /// Find every cross-file reference to the definition named `def_name`
    /// in the file at `def_uri`, across all currently open files.
    ///
    /// A cross-file reference is a multisym like `utils.greet` in a file that
    /// imports `def_uri` as the local binding `utils`.
    pub fn cross_file_refs_of(&self, def_uri: &Url, def_name: &str) -> Vec<CrossFileRef> {
        let def_path = def_uri.path();
        let mut out = Vec::new();

        for entry in self.files.iter() {
            let file = entry.value();
            if file.uri.path() == def_path {
                continue; // same-file refs are handled by the caller
            }
            for (binding, exports) in &file.modules {
                if exports.uri.path() != def_path {
                    continue;
                }
                // This file imports def_uri under the local name `binding`.
                for sym in &file.analysis.syms {
                    if sym.is_def {
                        continue;
                    }
                    if let Some(sep) = sym.name.find(['.', ':']) {
                        let root = &sym.name[..sep];
                        let member_full = &sym.name[sep + 1..];
                        let member_root = member_full
                            .split(['.', ':'])
                            .next()
                            .unwrap_or(member_full);
                        if root == binding && member_root == def_name {
                            out.push(CrossFileRef {
                                uri: file.uri.clone(),
                                text: file.text.clone(),
                                span: sym.span.clone(),
                                sym_name: sym.name.clone(),
                            });
                        }
                    }
                }
            }
        }

        out
    }

    /// Load and cache the top-level exports of a module file.
    ///
    /// Accepts both `.fnl` and `.lua` paths. Lua files are transpiled to Fennel
    /// before analysis; the resulting source map is stored in `ModuleSource::Lua`
    /// so that go-to-definition can remap Fennel spans back to Lua line numbers.
    ///
    /// Priority: open files (live version) > require_cache > disk.
    pub fn load_module(&self, path: &std::path::Path) -> Option<Arc<ModuleExports>> {
        let uri = Url::from_file_path(path).ok()?;

        // Open files take precedence: use the live in-memory version.
        // Open files are always Fennel (.fnl); Lua files are never tracked as open.
        if let Some(f) = self.files.get(&uri.to_string()) {
            return Some(Arc::new(ModuleExports {
                uri: f.uri.clone(),
                text: f.text.clone(),
                defs: top_level_defs(&f.analysis),
                source: ModuleSource::Fennel,
            }));
        }

        // Return cached analysis if available.
        if let Some(cached) = self.require_cache.get(path) {
            return Some(cached.clone());
        }

        // Read from disk.
        let disk_text = std::fs::read_to_string(path).ok()?;

        // For .lua files: transpile to Fennel first, keeping the source map.
        let (fennel_text, source) = if path.extension().map_or(false, |e| e == "lua") {
            match crate::lua_to_fennel::transpile(&disk_text) {
                Ok(out) => (out.fennel, ModuleSource::Lua { source_map: out.source_map }),
                Err(e) => {
                    log::warn!("lua→fennel transpile failed for {}: {e}", path.display());
                    return None;
                }
            }
        } else {
            (disk_text, ModuleSource::Fennel)
        };

        let (ast, _) = crate::parser::Parser::parse(&fennel_text);
        let analysis = crate::analyzer::analyze(&ast);

        let exports = Arc::new(ModuleExports {
            uri,
            text: fennel_text,
            defs: top_level_defs(&analysis),
            source,
        });
        self.require_cache.insert(path.to_path_buf(), exports.clone());
        Some(exports)
    }
}

/// Collect the names → DefinitionInfo for all bindings in the root scope (scope 0).
fn top_level_defs(analysis: &AnalysisResult) -> HashMap<String, DefinitionInfo> {
    let Some(root_scope) = analysis.scopes.first() else {
        return HashMap::new();
    };
    root_scope
        .bindings
        .values()
        .filter_map(|&def_byte| {
            analysis.defs.get(&def_byte).cloned().map(|d| (d.name.clone(), d))
        })
        .collect()
}

/// Resolve a Fennel module name (e.g. `"my.util"`) to a source file path
/// relative to `root`.
///
/// Search order: `.fnl` → `init.fnl` → `.lua` → `init.lua`.
/// Fennel takes priority so that a `.fnl` shadow of a `.lua` file is preferred.
pub fn resolve_require_path(
    module: &str,
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let rel = module.replace('.', "/");
    for suffix in &[".fnl", "/init.fnl", ".lua", "/init.lua"] {
        let candidate = root.join(format!("{rel}{suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default platform (Lua 5.4) ────────────────────────────────────────────

    #[test]
    fn default_workspace_knows_lua54_builtins() {
        let ws = Workspace::new();
        assert!(ws.builtins().is_known("print"));
        assert!(ws.builtins().is_known("pairs"));
        assert!(ws.builtins().is_known("io.open"),  "multisym via io root");
        assert!(ws.builtins().is_known("warn"),      "Lua 5.4 specific");
        assert!(ws.builtins().is_known("utf8"),      "Lua 5.3+ included in 5.4");
    }

    #[test]
    fn default_workspace_does_not_know_lua51_globals() {
        let ws = Workspace::new();
        // `unpack` is a bare global only in Lua 5.1; in 5.4 it lives in table.unpack
        assert!(!ws.builtins().is_known("unpack"), "unpack is Lua5.1-only");
    }

    #[test]
    fn default_workspace_does_not_know_luajit_globals() {
        let ws = Workspace::new();
        assert!(!ws.builtins().is_known("ffi"),  "ffi is LuaJIT-only");
        assert!(!ws.builtins().is_known("jit"),  "jit is LuaJIT-only");
        assert!(!ws.builtins().is_known("bit"),  "bit is LuaJIT-only");
    }

    // ── configure_platform ────────────────────────────────────────────────────

    #[test]
    fn configure_platform_lua51_has_unpack() {
        let ws = Workspace::new();
        ws.configure_platform(Platform::Lua51);
        assert!(ws.builtins().is_known("unpack"), "unpack global in Lua5.1");
        assert!(!ws.builtins().is_known("warn"),  "warn is Lua5.4-only");
    }

    #[test]
    fn configure_platform_luajit_has_ffi() {
        let ws = Workspace::new();
        ws.configure_platform(Platform::LuaJIT);
        assert!(ws.builtins().is_known("ffi"));
        assert!(ws.builtins().is_known("jit"));
        assert!(ws.builtins().is_known("bit"));
    }

    // ── with_builtins: Lua 5.1 ────────────────────────────────────────────────

    #[test]
    fn with_builtins_lua51_has_unpack() {
        let ws = Workspace::new().with_builtins(BuiltinSet::for_platform(Platform::Lua51));
        assert!(ws.builtins().is_known("unpack"), "unpack global in Lua5.1");
        assert!(!ws.builtins().is_known("warn"),  "warn is Lua5.4-only");
    }

    // ── with_builtins: LuaJIT ────────────────────────────────────────────────

    #[test]
    fn with_builtins_luajit_has_ffi_and_unpack() {
        let ws = Workspace::new().with_builtins(BuiltinSet::for_platform(Platform::LuaJIT));
        assert!(ws.builtins().is_known("ffi"),    "LuaJIT has ffi");
        assert!(ws.builtins().is_known("jit"),    "LuaJIT has jit");
        assert!(ws.builtins().is_known("bit"),    "LuaJIT has bit");
        assert!(ws.builtins().is_known("unpack"), "LuaJIT inherits Lua5.1 unpack");
    }

    // ── with_builtins: custom extras ─────────────────────────────────────────

    #[test]
    fn with_builtins_custom_extras_visible() {
        use crate::docs::{BuiltinDoc, BuiltinKind};
        let ws = Workspace::new().with_builtins(
            BuiltinSet::for_platform(Platform::Lua54).with_extra([(
                "love",
                BuiltinDoc { signature: "love".into(), doc: "LÖVE".into(), kind: BuiltinKind::Value },
            )]),
        );
        assert!(ws.builtins().is_known("love"),          "custom root");
        assert!(ws.builtins().is_known("love.graphics"), "custom multisym via root");
        assert!(ws.builtins().is_known("print"),         "built-ins still present");
    }

    // ── with_builtins replaces, not merges ────────────────────────────────────

    #[test]
    fn with_builtins_platform_switch_replaces_previous() {
        // After switching to Lua51, Lua54-only entries should NOT be present.
        let ws = Workspace::new().with_builtins(BuiltinSet::for_platform(Platform::Lua51));
        assert!(!ws.builtins().is_known("warn"),   "Lua54 warn gone after switch to Lua51");
        assert!(!ws.builtins().is_known("bit32"),  "Lua52 bit32 not in Lua51");
        // But shared builtins must still be there
        assert!(ws.builtins().is_known("print"));
        assert!(ws.builtins().is_known("unpack"));
    }

    // ── resolve_require_path ──────────────────────────────────────────────────

    #[test]
    fn resolve_require_path_direct_fnl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "").unwrap();
        let path = resolve_require_path("utils", dir.path()).unwrap();
        assert_eq!(path, dir.path().join("utils.fnl"));
    }

    #[test]
    fn resolve_require_path_init_fnl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("utils")).unwrap();
        std::fs::write(dir.path().join("utils/init.fnl"), "").unwrap();
        let path = resolve_require_path("utils", dir.path()).unwrap();
        assert_eq!(path, dir.path().join("utils/init.fnl"));
    }

    #[test]
    fn resolve_require_path_dotted_module() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("my")).unwrap();
        std::fs::write(dir.path().join("my/mod.fnl"), "").unwrap();
        let path = resolve_require_path("my.mod", dir.path()).unwrap();
        assert_eq!(path, dir.path().join("my/mod.fnl"));
    }

    #[test]
    fn resolve_require_path_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_require_path("nonexistent", dir.path()).is_none());
    }

    // ── load_module ───────────────────────────────────────────────────────────

    #[test]
    fn load_module_reads_top_level_defs() {
        let dir = tempfile::tempdir().unwrap();
        let fnl_path = dir.path().join("utils.fnl");
        std::fs::write(&fnl_path, "(fn helper [x y] x)").unwrap();
        let ws = Workspace::new();
        let exports = ws.load_module(&fnl_path).unwrap();
        assert!(exports.defs.contains_key("helper"), "helper should be a top-level def");
        let helper = &exports.defs["helper"];
        assert_eq!(helper.params, Some(vec!["x".to_string(), "y".to_string()]));
    }

    #[test]
    fn load_module_caches_result() {
        let dir = tempfile::tempdir().unwrap();
        let fnl_path = dir.path().join("mod.fnl");
        std::fs::write(&fnl_path, "(local x 1)").unwrap();
        let ws = Workspace::new();
        let e1 = ws.load_module(&fnl_path).unwrap();
        let e2 = ws.load_module(&fnl_path).unwrap();
        assert!(Arc::ptr_eq(&e1, &e2), "second call should return cached Arc");
    }

    #[test]
    fn update_resolves_require_bindings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn greet [name] name)").unwrap();

        let ws = Workspace::new();
        let uri = Url::parse("file:///test.fnl").unwrap();
        ws.update(
            uri.clone(),
            "(local utils (require :utils))\n(utils.greet \"world\")".to_string(),
            1,
            Some(dir.path()),
            &Default::default(),
        );

        let has_module = ws.with_file(&uri, |f| f.modules.contains_key("utils"));
        assert_eq!(has_module, Some(true));
    }

    #[test]
    fn update_without_root_no_modules() {
        let ws = Workspace::new();
        let uri = Url::parse("file:///test.fnl").unwrap();
        ws.update(
            uri.clone(),
            "(local x (require :utils))".to_string(),
            1,
            None,
            &Default::default(),
        );
        let module_count = ws.with_file(&uri, |f| f.modules.len());
        assert_eq!(module_count, Some(0));
    }

    // ── Lua module loading ────────────────────────────────────────────────────

    #[test]
    fn resolve_require_path_lua_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api.lua"), "").unwrap();
        let path = resolve_require_path("api", dir.path()).unwrap();
        assert_eq!(path, dir.path().join("api.lua"));
    }

    #[test]
    fn resolve_require_path_fnl_beats_lua() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api.fnl"), "").unwrap();
        std::fs::write(dir.path().join("api.lua"), "").unwrap();
        let path = resolve_require_path("api", dir.path()).unwrap();
        assert_eq!(path, dir.path().join("api.fnl"), ".fnl should take priority over .lua");
    }

    #[test]
    fn load_lua_module_exposes_top_level_defs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("api.lua"),
            "local function greet(name) return name end",
        ).unwrap();
        let ws = Workspace::new();
        let exports = ws.load_module(&dir.path().join("api.lua")).unwrap();
        assert!(exports.defs.contains_key("greet"), "greet should be a top-level def");
        assert!(matches!(exports.source, ModuleSource::Lua { .. }), "source should be Lua");
    }

    #[test]
    fn load_lua_module_source_map_points_to_lua_line() {
        let dir = tempfile::tempdir().unwrap();
        // line 1: local x = 1
        // line 2: local function greet(n) return n end
        std::fs::write(
            dir.path().join("api.lua"),
            "local x = 1\nlocal function greet(n) return n end",
        ).unwrap();
        let ws = Workspace::new();
        let exports = ws.load_module(&dir.path().join("api.lua")).unwrap();
        let def = exports.defs.get("greet").unwrap();
        let loc = exports.location_for_def(def);
        // greet is on Lua line 2 (1-indexed) → LSP line 1 (0-indexed)
        assert_eq!(loc.range.start.line, 1, "goto-def should land on Lua line 2 (LSP line 1)");
    }

    #[test]
    fn update_invalidates_require_cache() {
        let dir = tempfile::tempdir().unwrap();
        let fnl_path = dir.path().join("lib.fnl");
        std::fs::write(&fnl_path, "(fn old [] nil)").unwrap();

        let ws = Workspace::new();
        // Populate cache
        ws.load_module(&fnl_path).unwrap();
        assert!(ws.require_cache.contains_key(&fnl_path));

        // Update the file via the LSP — cache should be invalidated
        let uri = Url::from_file_path(&fnl_path).unwrap();
        ws.update(uri, "(fn new [] nil)".to_string(), 1, None, &Default::default());
        assert!(!ws.require_cache.contains_key(&fnl_path), "cache should be cleared on update");
    }

    // ── ModuleSource correctness ──────────────────────────────────────────────

    #[test]
    fn fnl_module_has_fennel_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.fnl"), "(fn foo [] nil)").unwrap();
        let ws = Workspace::new();
        let exports = ws.load_module(&dir.path().join("lib.fnl")).unwrap();
        assert!(matches!(exports.source, ModuleSource::Fennel), "source should be Fennel");
    }

    #[test]
    fn lua_module_has_lua_source_with_map() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.lua"), "local function f() end").unwrap();
        let ws = Workspace::new();
        let exports = ws.load_module(&dir.path().join("lib.lua")).unwrap();
        assert!(
            matches!(exports.source, ModuleSource::Lua { .. }),
            "source should be Lua with a source map"
        );
    }

    #[test]
    fn location_for_def_fennel_uses_direct_span() {
        let dir = tempfile::tempdir().unwrap();
        // `greet` definition starts at a known byte offset — span→range should
        // point directly to that position without any source-map remapping.
        std::fs::write(dir.path().join("util.fnl"), "(fn greet [n] n)").unwrap();
        let ws = Workspace::new();
        let exports = ws.load_module(&dir.path().join("util.fnl")).unwrap();
        let def = exports.defs.get("greet").unwrap();
        let loc = exports.location_for_def(def);
        // `greet` is the first token after `fn` at col 4 on line 0
        assert_eq!(loc.range.start.line, 0);
        assert_eq!(loc.range.start.character, 4, "greet starts at col 4 in '(fn greet [n] n)'");
    }

    // ── require chain: Fennel file requiring a Lua module ─────────────────────

    #[test]
    fn update_resolves_lua_require_binding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("api.lua"),
            "local function helper(x) return x end",
        ).unwrap();

        let ws = Workspace::new();
        let uri = Url::parse("file:///consumer.fnl").unwrap();
        ws.update(
            uri.clone(),
            "(local api (require :api))\n(api.helper 1)".to_string(),
            1,
            Some(dir.path()),
            &Default::default(),
        );

        let module_found = ws.with_file(&uri, |f| f.modules.contains_key("api"));
        assert_eq!(module_found, Some(true), "api module should be resolved");

        let def_found = ws
            .with_file(&uri, |f| {
                f.modules.get("api").map_or(false, |m| m.defs.contains_key("helper"))
            })
            .unwrap_or(false);
        assert!(def_found, "helper should be a def in the resolved Lua module");
    }

    #[test]
    fn lua_module_goto_def_remaps_to_lua_line() {
        let dir = tempfile::tempdir().unwrap();
        // Two functions: `f` on line 1, `g` on line 5 (after a blank line).
        std::fs::write(
            dir.path().join("lib.lua"),
            "local function f()\nend\n\n\nlocal function g()\nend",
        ).unwrap();

        let ws = Workspace::new();
        let exports = ws.load_module(&dir.path().join("lib.lua")).unwrap();

        let f_loc = exports.location_for_def(exports.defs.get("f").unwrap());
        let g_loc = exports.location_for_def(exports.defs.get("g").unwrap());

        assert_eq!(f_loc.range.start.line, 0, "f is on Lua line 1 → LSP line 0");
        assert_eq!(g_loc.range.start.line, 4, "g is on Lua line 5 → LSP line 4");
    }

    #[test]
    fn lua_module_cached_as_lua_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cached.lua");
        std::fs::write(&path, "local function x() end").unwrap();

        let ws = Workspace::new();
        let e1 = ws.load_module(&path).unwrap();
        let e2 = ws.load_module(&path).unwrap();

        assert!(Arc::ptr_eq(&e1, &e2), "second load should return the cached Arc");
        assert!(matches!(e2.source, ModuleSource::Lua { .. }));
    }

    #[test]
    fn all_defs_includes_lua_module_defs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mylib.lua");
        std::fs::write(&path, "local function exported_fn() end").unwrap();

        let ws = Workspace::new();
        // Populate the require cache
        ws.load_module(&path).unwrap();

        let defs = ws.all_defs("exported_fn");
        assert!(!defs.is_empty(), "all_defs should find Lua module defs in the require cache");
        assert!(defs.iter().any(|(_, _, d)| d.name == "exported_fn"));
    }

    // ── nested require: Fennel → Fennel → Lua ────────────────────────────────

    #[test]
    fn nested_require_fnl_then_lua() {
        let dir = tempfile::tempdir().unwrap();

        // lua-lib.lua: the Lua leaf module
        std::fs::write(
            dir.path().join("lua-lib.lua"),
            "local function compute(x) return x * 2 end",
        ).unwrap();

        // middle.fnl: Fennel wrapper that requires lua-lib
        std::fs::write(
            dir.path().join("middle.fnl"),
            "(local lib (require :lua-lib))\n(fn wrap [x] (lib.compute x))",
        ).unwrap();

        let ws = Workspace::new();

        // Open middle.fnl — it should resolve lua-lib as a Lua module
        let middle_uri = Url::parse("file:///middle.fnl").unwrap();
        ws.update(
            middle_uri.clone(),
            std::fs::read_to_string(dir.path().join("middle.fnl")).unwrap(),
            1,
            Some(dir.path()),
            &Default::default(),
        );

        let lua_module_resolved = ws
            .with_file(&middle_uri, |f| {
                f.modules.get("lib").map_or(false, |m| matches!(m.source, ModuleSource::Lua { .. }))
            })
            .unwrap_or(false);
        assert!(lua_module_resolved, "middle.fnl should resolve lua-lib.lua as a Lua module");

        // The Lua module's `compute` def should be reachable for goto-def
        let compute_found = ws
            .with_file(&middle_uri, |f| {
                f.modules.get("lib").map_or(false, |m| m.defs.contains_key("compute"))
            })
            .unwrap_or(false);
        assert!(compute_found, "compute should be findable in lib module");

        // goto-def on lib.compute should remap to Lua line 1 (LSP line 0)
        let lua_line = ws.with_file(&middle_uri, |f| {
            let lib = f.modules.get("lib")?;
            let def = lib.defs.get("compute")?;
            Some(lib.location_for_def(def).range.start.line)
        });
        assert_eq!(lua_line, Some(Some(0)), "compute is on Lua line 1 → LSP line 0");
    }
}
