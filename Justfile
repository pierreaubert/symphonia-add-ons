# --------------------------------------------------------- -*- just -*-
# Symphonia Add-ons — workspace tasks
# How to install Just?
#	  cargo install just
# ----------------------------------------------------------------------

_default:
	just --list

# ----------------------------------------------------------------------
# VARIABLES
# ----------------------------------------------------------------------

test_features := ""

# ----------------------------------------------------------------------
# TEST
# ----------------------------------------------------------------------

[group('test')]
check:
	cargo check --workspace --lib --bins --tests --examples {{test_features}}

[group('test')]
test:
	cargo test --workspace --lib --bins --tests --examples {{test_features}}

# ----------------------------------------------------------------------
# LINT
# ----------------------------------------------------------------------

[group('lint')]
lint:
	cargo clippy --workspace --all-targets -- -D warnings

# ----------------------------------------------------------------------
# FORMAT
# ----------------------------------------------------------------------

alias format := fmt

fmt:
	cargo fmt --all

# ----------------------------------------------------------------------
# CLEAN
# ----------------------------------------------------------------------

clean:
	cargo clean
	find . -name '*~' -exec rm {} \; -print
	find . -name 'Cargo.lock' -path '*/target/*' -prune -o -name 'Cargo.lock' -exec rm {} \; -print
