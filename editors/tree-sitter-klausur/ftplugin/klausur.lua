vim.bo.commentstring = "// %s"
vim.bo.comments = "s1:/*,mb:*,ex:*/,:///,://"

-- The parser is `parser/klausur.so` in this plugin (`make` builds it).
if not pcall(vim.treesitter.start) then
  vim.notify_once("klausur: no tree-sitter parser; run `make` in the plugin directory", vim.log.levels.WARN)
end
