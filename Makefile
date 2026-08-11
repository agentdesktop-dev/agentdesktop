.PHONY: gen generate-schema format

gen: generate-schema format
	@:

generate-schema:
	@cargo xtask schema

format:
	@cargo fmt --all
