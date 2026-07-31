# Tymbolica

Tymbolica exposes Symbolica's computer algebra API to Typst through a
`wasm32-unknown-unknown` plugin. The Rust plugin and Typst interface are kept in
this standalone project and depend on Symbolica's upstream `dev` branch with its
`wasm` feature.

Build the plugin:

```sh
nix run .#build
```

Compile every example and the Tidy manual:

```sh
nix run .#check
```

Generate the manual:

```sh
nix run .#manual
```

The generated `typst/tymbolica.wasm` is intentionally untracked. Symbolica is
source-available and its redistribution terms apply to a bundled Wasm binary;
see <https://symbolica.io/license/>.
