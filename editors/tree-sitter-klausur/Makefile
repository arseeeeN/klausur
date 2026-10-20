# Builds the Neovim parser from the generated C source; needs only a C compiler.
# `make generate` regenerates src/ from grammar.js and needs `npm install` first.

CC ?= cc

parser/klausur.so: src/parser.c
	mkdir -p parser
	$(CC) -O2 -shared -fPIC -Isrc src/parser.c -o $@

generate:
	npx tree-sitter generate

test: generate
	npx tree-sitter test

clean:
	rm -rf parser

.PHONY: generate test clean
