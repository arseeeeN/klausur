# tree-sitter-klausur

A [tree-sitter](https://tree-sitter.github.io) grammar for klausur files (`.kl`), with highlight,
fold and indent queries. It extends
[tree-sitter-rhai](https://github.com/elkowar/tree-sitter-rhai) (vendored in `rhai/`, MIT), so
`fn`s and test bodies parse as Rhai. It adds requests and their sections, tests, modifiers,
`expect`, `is`, `null`, named arguments and parameter defaults.

## Neovim

This directory is also a Neovim plugin: a filetype rule for `*.kl`, an ftplugin that starts
tree-sitter, and the queries under `queries/klausur/`. Neovim 0.11 loads the parser from
`parser/klausur.so` on the runtime path, so nvim-treesitter doesn't need to know about it.
Building it needs only a C compiler. With lazy.nvim:

```lua
{
    dir = "~/Coding/klausur/editors/tree-sitter-klausur",
    name = "klausur",
    build = "make",
    lazy = false,
}
```

After changing the grammar, run `:Lazy build klausur`, or `make` in this directory.

## Working on the grammar

```bash
npm install          # the tree-sitter CLI
make generate        # grammar.js → src/parser.c (committed, so users don't need node)
make test            # the corpus in test/corpus
npx tree-sitter parse ../../path/to/file.kl
npx tree-sitter query queries/klausur/highlights.scm file.kl   # which captures apply where
```

Notes on the grammar:
- The root node is called `Rhai`: tree-sitter starts at the base grammar's first rule.
- Plain names parse as `ExprPath → Path → ident`, as in tree-sitter-rhai.
- A `connection` block isn't parsed by line. Its URL and header values are whitespace-free
  `text` words and `{{…}}` interpolations, and a header starts at a `Name: ` token, colon and
  space included. That's how `https://…` stays a URL.
- Comments rank above operators, unlike in tree-sitter-rhai, where `x // note` parses the
  comment as two `/` operators. Text in JSON strings and ``` bodies ranks above comments, so
  `//` stays literal there.
