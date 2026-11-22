# Simple wasm grammar fixture

This minimal grammar exercises `Grammar::from_wasm` in `wasm_grammar.rs`. It
paints a word as a variable and a number as a number.

Rebuild `simple.wasm` after changing `grammar.js` or the query:

```sh
tree-sitter generate
tree-sitter build --wasm -o simple.wasm
rm -rf src
```

The test loads `simple.wasm` and `queries/highlights.scm` with `include_bytes!`
and `include_str!`. The generated `src/` tree is not committed.
