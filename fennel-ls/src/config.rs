/// User configuration loaded from `.fennel-ls.toml` in the workspace root.

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Target Lua platform: "lua51", "lua52", "lua53", "lua54" (default), "luajit", "luau"
    pub platform: Option<String>,
    /// Extra global names that should not produce unknown-identifier warnings.
    pub known_globals: Option<Vec<String>>,
}

impl Config {
    pub fn load(root: &std::path::Path) -> Self {
        let path = root.join(".fennel-ls.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Self::default(),
        };
        toml::from_str(&text).unwrap_or_default()
    }
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
}
