{
  description = "Symbolica computer algebra for Typst";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
    in {
      devShells = eachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.binaryen pkgs.cargo pkgs.lld pkgs.rustc pkgs.typst ];
        };
      });

      apps = eachSystem (pkgs:
        let
          path = [ pkgs.binaryen pkgs.cargo pkgs.coreutils pkgs.lld pkgs.rustc pkgs.typst ];
          app = name: text: {
            type = "app";
            program = "${pkgs.writeShellApplication { inherit name; runtimeInputs = path; inherit text; }}/bin/${name}";
          };
          buildScript = ''
            target=wasm32-unknown-unknown
            unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
            cargo build --release --target "$target"
            wasm-opt -Oz --quiet --enable-bulk-memory --enable-bulk-memory-opt --enable-nontrapping-float-to-int --strip-debug --strip-producers \
              -o typst/tymbolica.wasm "target/$target/release/tymbolica_plugin.wasm"
            ls -lh typst/tymbolica.wasm
          '';
        in rec {
          default = build;
          build = app "tymbolica-build" buildScript;
          manual = app "tymbolica-manual" (buildScript + ''
            out="''${TYMBOLICA_MANUAL_OUT:-dist/tymbolica-manual.pdf}"
            if [ "$#" -gt 0 ]; then
              out="$1"
              shift
            fi
            mkdir -p "$(dirname "$out")"
            typst compile --root typst typst/manual.typ "$out" "$@"
            ls -lh "$out"
          '');
          check = app "tymbolica-check" (buildScript + ''
            typst compile --root typst typst/examples/basic.typ /tmp/tymbolica-basic.pdf
            typst compile --root typst typst/examples/api-surface.typ /tmp/tymbolica-api-surface.pdf
            typst compile --root typst typst/examples/parsely-mwe.typ /tmp/tymbolica-parsely.pdf
            typst compile --root typst typst/manual.typ /tmp/tymbolica-manual.pdf
          '');
          typst = app "tymbolica-typst" ''exec typst "$@"'';
        });
    };
}
