/// User configuration loaded from `.fennel-ls.toml` in the workspace root.
///
/// # Quick reference
///
/// ```toml
/// platform = "luajit"
/// known_globals = ["state"]
/// include = ["path/to/my-api.toml"]
///
/// [global_docs."MyLib.do_thing"]
/// signature = "(MyLib.do_thing arg1 arg2)"
/// doc = "Does the thing."
/// ```
///
/// See the **Configuration** section of ARCHITECTURE.md for full details.

use std::collections::HashMap;
use serde::Deserialize;
use mlua::prelude::*;

/// Documentation entry for a single global name.
///
/// Used in `[global_docs]` sections — either inline in `.fennel-ls.toml`
/// or in an included file. Both fields mirror the layout used for built-in
/// docs so hover output is consistent.
#[derive(Debug, Default, Deserialize)]
pub struct GlobalDoc {
    /// Short fennel-style call signature shown in the hover code block.
    /// Example: `"(MyLib.module.fn arg1 arg2)"`
    pub signature: String,
    /// Prose description shown below the signature. Supports Markdown.
    pub doc: Option<String>,
}

/// Parsed contents of an include file (a TOML file that only contributes
/// `[global_docs]` entries and nothing else).
#[derive(Debug, Default, Deserialize)]
struct IncludeFile {
    global_docs: Option<HashMap<String, GlobalDoc>>,
}

/// Full user configuration.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Target Lua platform: "lua51", "lua52", "lua53", "lua54" (default), "luajit", "luau"
    pub platform: Option<String>,

    /// Extra global names that suppress unknown-identifier warnings but have
    /// no hover documentation.  Roots inferred from `global_docs` keys are
    /// added automatically, so you only need this for undocumented globals
    /// (e.g. `known_globals = ["state"]`).
    #[serde(alias = "known-globals")]
    pub known_globals: Option<Vec<String>>,

    /// Paths (relative to the workspace root) of extra TOML files whose
    /// `[global_docs]` sections are merged into this config.
    /// Useful for keeping engine/framework API docs in one shared file and
    /// referencing them from multiple per-project `.fennel-ls.toml` files.
    ///
    /// Example:
    /// ```toml
    /// include = ["../../my-engine/api-docs.toml"]
    /// ```
    pub include: Option<Vec<String>>,

    /// Inline documentation for global names (functions, namespaces, values).
    /// Keys are the exact Fennel symbol as it appears in source, including
    /// dots for namespaced APIs (e.g. `"MyLib.module.fn"`).
    ///
    /// Roots are extracted automatically and added to the known-globals set,
    /// so you do not need to repeat them in `known_globals`.
    ///
    /// Hover: the server does an exact-match lookup first, then strips
    /// trailing `.member` segments until it finds a match or exhausts the
    /// chain.  This means a single parent namespace entry acts as a fallback
    /// for any child call that has no specific entry.
    #[serde(alias = "global-docs")]
    pub global_docs: Option<HashMap<String, GlobalDoc>>,
}

impl Config {
    /// Load configuration from `<root>/.lsp.fnl` (preferred) or
    /// `<root>/.fennel-ls.toml` (fallback).
    pub fn load(root: &std::path::Path) -> Self {
        let fnl_path = root.join(".lsp.fnl");
        log::info!("Config::load: checking for {}", fnl_path.display());
        if fnl_path.exists() {
            log::info!("Config::load: found .lsp.fnl, evaluating...");
            match load_fnl(&fnl_path, root) {
                Ok(config) => {
                    log::info!("Config::load: .lsp.fnl loaded successfully");
                    return config;
                }
                Err(e) => log::warn!("Config::load: .lsp.fnl failed to load: {e}; falling back to .fennel-ls.toml"),
            }
        } else {
            log::info!("Config::load: .lsp.fnl not found");
        }

        let path = root.join(".fennel-ls.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Self::default(),
        };
        let mut config: Config = toml::from_str(&text).unwrap_or_default();

        // Merge included doc files into global_docs
        if let Some(includes) = config.include.take() {
            let mut all_docs = config.global_docs.take().unwrap_or_default();
            for rel in &includes {
                let inc_path = root.join(rel);
                if let Ok(text) = std::fs::read_to_string(&inc_path) {
                    match toml::from_str::<IncludeFile>(&text) {
                        Ok(inc) => { if let Some(docs) = inc.global_docs { all_docs.extend(docs); } }
                        Err(e) => log::warn!("include file {} failed to parse: {e}", inc_path.display()),
                    }
                }
            }
            if !all_docs.is_empty() {
                config.global_docs = Some(all_docs);
            }
        }

        config
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

    let eval_fn: LuaFunction = fennel.get("eval")?;
    let opts = lua.create_table()?;
    opts.set("filename", path.to_string_lossy().as_ref())?;

    log::debug!("load_fnl: calling fennel.eval on {}", path.display());
    let result: LuaValue = eval_fn.call((src, opts)).map_err(|e| {
        log::warn!("load_fnl: fennel.eval failed: {e}");
        e
    })?;

    log::debug!("load_fnl: deserializing result");
    lua.from_value(result).map_err(|e| {
        log::warn!("load_fnl: deserialization failed: {e}");
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn empty_config_is_all_none() {
        let c = parse("");
        assert!(c.platform.is_none());
        assert!(c.known_globals.is_none());
        assert!(c.include.is_none());
        assert!(c.global_docs.is_none());
    }

    #[test]
    fn platform_field_parsed() {
        let c = parse(r#"platform = "luajit""#);
        assert_eq!(c.platform.as_deref(), Some("luajit"));
    }

    #[test]
    fn known_globals_parsed() {
        let c = parse(r#"known_globals = ["love", "vim", "hs"]"#);
        let g = c.known_globals.unwrap();
        assert!(g.contains(&"love".to_string()));
        assert!(g.contains(&"vim".to_string()));
        assert!(g.contains(&"hs".to_string()));
    }

    #[test]
    fn platform_and_globals_together() {
        let c = parse(r#"
platform = "lua51"
known_globals = ["my_lib"]
"#);
        assert_eq!(c.platform.as_deref(), Some("lua51"));
        assert_eq!(c.known_globals.unwrap(), vec!["my_lib"]);
    }

    #[test]
    fn invalid_toml_returns_default() {
        let c: Config = toml::from_str("not valid {{{{").unwrap_or_default();
        assert!(c.platform.is_none());
    }

    #[test]
    fn include_field_parsed() {
        let c = parse(r#"include = ["../../my-api.toml", "extra.toml"]"#);
        let inc = c.include.unwrap();
        assert_eq!(inc, vec!["../../my-api.toml", "extra.toml"]);
    }

    #[test]
    fn global_docs_inline_parsed() {
        let c = parse(r#"
[global_docs."MyLib.module.fn"]
signature = "(MyLib.module.fn cols rows)"
doc = "Does something."
"#);
        let docs = c.global_docs.unwrap();
        let entry = docs.get("MyLib.module.fn").unwrap();
        assert_eq!(entry.signature, "(MyLib.module.fn cols rows)");
        assert_eq!(entry.doc.as_deref(), Some("Does something."));
    }

    #[test]
    fn global_docs_doc_field_is_optional() {
        let c = parse(r#"
[global_docs."MyLib.fn"]
signature = "(MyLib.fn x)"
"#);
        let docs = c.global_docs.unwrap();
        let entry = docs.get("MyLib.fn").unwrap();
        assert!(entry.doc.is_none());
    }

    #[test]
    fn global_docs_merge_from_include_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Write the included file
        let inc_path = dir.path().join("api.toml");
        let mut f = std::fs::File::create(&inc_path).unwrap();
        writeln!(f, r#"
[global_docs."Engine.tick"]
signature = "(Engine.tick dt)"
doc = "Called every frame."
"#).unwrap();

        // Write the base config
        let base_path = dir.path().join(".fennel-ls.toml");
        let mut f = std::fs::File::create(&base_path).unwrap();
        writeln!(f, r#"include = ["api.toml"]"#).unwrap();

        let config = Config::load(dir.path());
        let docs = config.global_docs.unwrap();
        let entry = docs.get("Engine.tick").unwrap();
        assert_eq!(entry.signature, "(Engine.tick dt)");
        assert_eq!(entry.doc.as_deref(), Some("Called every frame."));
    }

    #[test]
    fn include_missing_file_is_silently_skipped() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join(".fennel-ls.toml");
        let mut f = std::fs::File::create(&base_path).unwrap();
        writeln!(f, r#"include = ["does-not-exist.toml"]"#).unwrap();
        // Should not panic; global_docs stays empty
        let config = Config::load(dir.path());
        assert!(config.global_docs.is_none());
    }

    #[test]
    fn inline_and_included_docs_are_merged() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let inc_path = dir.path().join("extra.toml");
        let mut f = std::fs::File::create(&inc_path).unwrap();
        writeln!(f, r#"
[global_docs."Lib.from_include"]
signature = "(Lib.from_include)"
"#).unwrap();

        let base_path = dir.path().join(".fennel-ls.toml");
        let mut f = std::fs::File::create(&base_path).unwrap();
        writeln!(f, r#"
include = ["extra.toml"]

[global_docs."Lib.inline"]
signature = "(Lib.inline)"
"#).unwrap();

        let config = Config::load(dir.path());
        let docs = config.global_docs.unwrap();
        assert!(docs.contains_key("Lib.from_include"), "missing include entry");
        assert!(docs.contains_key("Lib.inline"), "missing inline entry");
    }
}
