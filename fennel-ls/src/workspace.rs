/// File state management. Each open file is parsed and analyzed on every change.

use std::sync::{Arc, OnceLock};
use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

use crate::docs::{BuiltinSet, Platform};
use crate::parser::{AstNode, ParseError};
use crate::analyzer::AnalysisResult;

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
}

#[derive(Clone)]
pub struct Workspace {
    files: Arc<DashMap<String, AnalyzedFile>>,
    // OnceLock so configure_platform() can be called from &self in initialize().
    builtins: Arc<OnceLock<Arc<BuiltinSet>>>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            files: Arc::default(),
            builtins: Arc::new(OnceLock::new()),
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

    pub fn update(&self, uri: Url, text: String, version: i32) {
        let (ast, parse_errors) = crate::parser::Parser::parse(&text);
        let analysis = crate::analyzer::analyze(&ast);

        self.files.insert(
            uri.to_string(),
            AnalyzedFile {
                uri,
                text,
                version,
                ast,
                parse_errors,
                analysis,
            },
        );
    }

    pub fn remove(&self, uri: &Url) {
        self.files.remove(&uri.to_string());
    }

    /// Run `f` with a reference to the file, returning `None` if not found.
    pub fn with_file<F, R>(&self, uri: &Url, f: F) -> Option<R>
    where
        F: FnOnce(&AnalyzedFile) -> R,
    {
        let entry = self.files.get(&uri.to_string())?;
        Some(f(&*entry))
    }
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
}
