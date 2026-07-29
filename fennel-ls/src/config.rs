use std::collections::HashMap;
use serde::Deserialize;
use mlua::prelude::*;
use mlua::serde::de::{Deserializer as LuaDeserializer, Options as LuaDeOptions};

/// Documentation entry for a single global name, loaded from `.lsp.fnl`.
#[derive(Debug, Default, Deserialize)]
pub struct GlobalDoc {
    pub signature: String,
    pub doc: Option<String>,
}

/// Full user configuration, loaded from `.lsp.fnl` in the workspace root.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Target Lua platform: "lua51", "lua52", "lua53", "lua54" (default), "luajit", "luau"
    pub platform: Option<String>,

    /// Extra global names that suppress unknown-identifier warnings but have no hover docs.
    /// Roots inferred from `global_docs` keys are added automatically.
    #[serde(alias = "known-globals")]
    pub known_globals: Option<Vec<String>>,

    /// Per-symbol hover documentation. Keys are exact Fennel symbols (dots for namespaces).
    #[serde(alias = "global-docs")]
    pub global_docs: Option<HashMap<String, GlobalDoc>>,

    /// Emit a hint-level diagnostic on macro calls that have no hook defined.
    /// Default: false. Enable with `:warn-unhooked-macros true` in `.lsp.fnl`.
    #[serde(alias = "warn-unhooked-macros")]
    pub warn_unhooked_macros: Option<bool>,

    /// Macros that are globally available in every file without import-macros.
    /// Keys are module paths, values are lists of macro names from that module.
    /// Example: `{:global-macros {"addons.fennel-gdextension.defnode" [:defnode]}}`
    #[serde(alias = "global-macros")]
    pub global_macros: Option<HashMap<String, Vec<String>>>,
}

impl Config {
    /// Load configuration from `<root>/.lsp.fnl`. Returns default if absent or on error.
    pub fn load(root: &std::path::Path) -> Self {
        let fnl_path = root.join(".lsp.fnl");
        log::info!("Config::load: checking for {}", fnl_path.display());
        if !fnl_path.exists() {
            log::info!("Config::load: .lsp.fnl not found");
            return Self::default();
        }
        log::info!("Config::load: found .lsp.fnl, evaluating...");
        match load_fnl(&fnl_path, root) {
            Ok(config) => {
                log::info!("Config::load: .lsp.fnl loaded successfully");
                config
            }
            Err(e) => {
                log::warn!("Config::load: .lsp.fnl failed to load: {e}");
                Self::default()
            }
        }
    }
}

const FENNEL_SRC: &str = include_str!("../vendor/fennel.lua");

fn load_fnl(path: &std::path::Path, root: &std::path::Path) -> LuaResult<Config> {
    let src = std::fs::read_to_string(path)
        .map_err(LuaError::external)?;

    let lua = unsafe { Lua::unsafe_new() };

    let fennel: LuaTable = lua.load(FENNEL_SRC).set_name("fennel.lua").eval()?;

    // Extend fennel.path so require works from the project root
    let root_str = root.to_string_lossy();
    let orig_path: String = fennel.get("path").unwrap_or_default();
    let new_path = format!("{}/?.fnl;{}/?/init.fnl;{}", root_str, root_str, orig_path);
    log::debug!("load_fnl: fennel.path = {}", new_path);
    fennel.set("path", new_path)?;

    lua.globals().set("fennel", fennel.clone())?;

    // Install Fennel's require searcher so (require :some.module) resolves
    // .fnl files via fennel.path rather than Lua's native .lua-only searcher.
    let install_fn: LuaFunction = fennel.get("install")?;
    install_fn.call::<_, ()>(())?;

    let eval_fn: LuaFunction = fennel.get("eval")?;
    let opts = lua.create_table()?;
    opts.set("filename", path.to_string_lossy().as_ref())?;

    log::debug!("load_fnl: calling fennel.eval on {}", path.display());
    let result: LuaValue = eval_fn.call((src, opts)).map_err(|e| {
        log::warn!("load_fnl: fennel.eval failed: {e}");
        e
    })?;

    log::debug!("load_fnl: deserializing result");
    // Use deny_unsupported_types=false so Lua functions (e.g. from :macro-hooks)
    // are silently skipped rather than causing the entire deserialization to fail.
    let de = LuaDeserializer::new_with_options(
        result,
        LuaDeOptions::new().deny_unsupported_types(false),
    );
    Config::deserialize(de).map_err(|e| {
        log::warn!("load_fnl: deserialization failed: {e}");
        LuaError::external(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lsp_fnl(dir: &std::path::Path, src: &str) {
        std::fs::write(dir.join(".lsp.fnl"), src).unwrap();
    }

    #[test]
    fn missing_config_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path());
        assert!(config.platform.is_none());
        assert!(config.known_globals.is_none());
        assert!(config.global_docs.is_none());
    }

    #[test]
    fn empty_table_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), "{}");
        let config = Config::load(dir.path());
        assert!(config.platform.is_none());
        assert!(config.known_globals.is_none());
        assert!(config.global_docs.is_none());
    }

    #[test]
    fn platform_field_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), r#"{:platform "luajit"}"#);
        let config = Config::load(dir.path());
        assert_eq!(config.platform.as_deref(), Some("luajit"));
    }

    #[test]
    fn known_globals_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), r#"{:known-globals ["love" "vim" "hs"]}"#);
        let config = Config::load(dir.path());
        let g = config.known_globals.unwrap();
        assert!(g.contains(&"love".to_string()));
        assert!(g.contains(&"vim".to_string()));
        assert!(g.contains(&"hs".to_string()));
    }

    #[test]
    fn platform_and_globals_together() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), r#"{:platform "lua51" :known-globals ["my_lib"]}"#);
        let config = Config::load(dir.path());
        assert_eq!(config.platform.as_deref(), Some("lua51"));
        assert_eq!(config.known_globals.unwrap(), vec!["my_lib"]);
    }

    #[test]
    fn invalid_fennel_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), "this is not valid fennel {{{{");
        let config = Config::load(dir.path());
        assert!(config.platform.is_none());
    }

    #[test]
    fn global_docs_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), r#"
{:global-docs {"MyLib.module.fn" {:signature "(MyLib.module.fn cols rows)"
                                   :doc "Does something."}}}
"#);
        let config = Config::load(dir.path());
        let docs = config.global_docs.unwrap();
        let entry = docs.get("MyLib.module.fn").unwrap();
        assert_eq!(entry.signature, "(MyLib.module.fn cols rows)");
        assert_eq!(entry.doc.as_deref(), Some("Does something."));
    }

    #[test]
    fn global_docs_doc_field_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        write_lsp_fnl(dir.path(), r#"
{:global-docs {"MyLib.fn" {:signature "(MyLib.fn x)"}}}
"#);
        let config = Config::load(dir.path());
        let docs = config.global_docs.unwrap();
        let entry = docs.get("MyLib.fn").unwrap();
        assert!(entry.doc.is_none());
    }

    #[test]
    fn require_merges_docs_from_another_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api.fnl"), r#"
{"Engine.tick" {:signature "(Engine.tick dt)" :doc "Called every frame."}}
"#).unwrap();
        write_lsp_fnl(dir.path(), r#"
(local api (require :api))
{:global-docs api}
"#);
        let config = Config::load(dir.path());
        let docs = config.global_docs.unwrap();
        let entry = docs.get("Engine.tick").unwrap();
        assert_eq!(entry.signature, "(Engine.tick dt)");
        assert_eq!(entry.doc.as_deref(), Some("Called every frame."));
    }
}
