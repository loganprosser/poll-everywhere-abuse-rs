.PHONY: help app run setup test

help:  ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-8s %s\n", $$1, $$2}'

setup: ## One-time: Rust + Chrome check + AI CLI check + release build
	./scripts/setup.sh && mkdir -p target && touch target/.installed

# app/run/test run setup first when it hasn't happened yet (or
# Cargo.toml changed), so a fresh clone needs just `make app`.
target/.installed: Cargo.toml
	./scripts/setup.sh && mkdir -p target && touch target/.installed

# Build dist/Poll Everywhere Abuse.app locally (ad-hoc signed; locally-built
# apps carry no quarantine attribute).
app: target/.installed  ## Build dist/Poll Everywhere Abuse.app
	./scripts/build-app.sh

# Run the GUI from source, no packaging.
run: target/.installed  ## Launch the GUI from source
	cargo run --release

test: target/.installed  ## Offline tests (no network, no AI, nothing sent)
	cargo test --release
