{
  description = "fennel-ls-rs — Fennel language server (Rust) + tree-sitter grammar";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Lua runtime used by the embedded Fennel evaluator (config + macro expansion).
      # This has no effect on the Lua version your project targets — it is tooling only.
      # Valid values: "luajit" (default), "lua54", "lua53", "lua52", "lua51"
      lua-runtime = "luajit";
    in {
      packages = forAllSystems (pkgs: rec {
        fennel-ls = pkgs.rustPlatform.buildRustPackage {
          pname = "fennel-ls";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "fennel-ls" "--no-default-features" "--features" lua-runtime ];
        };

        tree-sitter-fennel = pkgs.tree-sitter.buildGrammar {
          language = "fennel";
          version = "0.0.1";
          src = self + "/tree-sitter-fennel";
        };

        default = fennel-ls;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustup
            nodejs
            tree-sitter
          ];

          shellHook = ''
            echo "fennel-ls-rs dev shell"
            echo "  cargo build / cargo test   — LSP crate"
            echo "  cd tree-sitter-fennel"
            echo "  npm install && tree-sitter generate   — regenerate parser.c after grammar changes"
          '';
        };
      });
    };
}
