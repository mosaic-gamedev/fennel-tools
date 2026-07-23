/// File state management. Each open file is parsed and analyzed on every change.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

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

/// Top-level exports from a required module file.
#[derive(Debug)]
pub struct ModuleExports {
    pub uri: Url,
    /// Source text of the module (needed for span→range conversion in goto_definition).
    pub text: String,
    /// Top-level definitions, keyed by name.
    pub defs: HashMap<String, DefinitionInfo>,
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
    pub fn update(
        &self,
        uri: Url,
        text: String,
        version: i32,
        workspace_root: Option<&std::path::Path>,
    ) {
        // Invalidate the require_cache entry for this file so callers get
        // fresh exports after edits.
        if let Ok(path) = uri.to_file_path() {
            self.require_cache.remove(&path);
        }

        let (ast, parse_errors) = crate::parser::Parser::parse(&text);
        let analysis = crate::analyzer::analyze(&ast);

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
    /// Called asynchronously after initial analysis; triggers a diagnostic re-publish.
    pub fn set_macro_globals(&self, uri: &Url, names: HashSet<String>) {
        if let Some(mut entry) = self.files.get_mut(&uri.to_string()) {
            entry.macro_globals = names;
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
    /// Priority: open files (live version) > require_cache > disk.
    /// Reading from disk without a workspace root is intentional — callers
    /// supply the resolved absolute path.
    pub fn load_module(&self, path: &std::path::Path) -> Option<Arc<ModuleExports>> {
        let uri = Url::from_file_path(path).ok()?;

        // Open files take precedence: use the live in-memory version.
        if let Some(f) = self.files.get(&uri.to_string()) {
            return Some(Arc::new(ModuleExports {
                uri: f.uri.clone(),
                text: f.text.clone(),
                defs: top_level_defs(&f.analysis),
            }));
        }

        // Return cached analysis if available.
        if let Some(cached) = self.require_cache.get(path) {
            return Some(cached.clone());
        }

        // Read from disk, parse, analyze, cache.
        let text = std::fs::read_to_string(path).ok()?;
        let (ast, _) = crate::parser::Parser::parse(&text);
        let analysis = crate::analyzer::analyze(&ast);

        let exports = Arc::new(ModuleExports {
            uri,
            text,
            defs: top_level_defs(&analysis),
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

/// Resolve a Fennel module name (e.g. `"my.util"`) to a `.fnl` file path
/// relative to `root`. Tries `mod/name.fnl` then `mod/name/init.fnl`.
pub fn resolve_require_path(
    module: &str,
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let rel = module.replace('.', "/");
    for suffix in &[".fnl", "/init.fnl"] {
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
        );
        let module_count = ws.with_file(&uri, |f| f.modules.len());
        assert_eq!(module_count, Some(0));
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
        ws.update(uri, "(fn new [] nil)".to_string(), 1, None);
        assert!(!ws.require_cache.contains_key(&fnl_path), "cache should be cleared on update");
    }
}
