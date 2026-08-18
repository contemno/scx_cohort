# One source of truth for the file lists CI, the release build, and the
# pre-push hook check — every entry point calls these targets identically
# instead of hand-duplicating the lists in workflow YAML. See
# repo_standard's STANDARD.md -> "One list of checked files, not several".

PY_SCRIPTS := bench/analyze.py bench/test_analyze.py
SH_SCRIPTS := bench/run-suite.sh scripts/install-hooks.sh scripts/next-version.sh

.PHONY: lint lint-fast fmt-check lint-rust lint-scripts test

# Everything CI gates on.
lint: lint-rust lint-scripts

# Pre-push subset: seconds, not minutes (clippy needs a full BPF build).
lint-fast: fmt-check lint-scripts

fmt-check:
	cargo fmt --all --check

lint-rust: fmt-check
	cargo clippy --workspace --all-targets -- -D warnings

lint-scripts:
	python3 -m py_compile $(PY_SCRIPTS)
	@for f in $(SH_SCRIPTS); do bash -n "$$f" || exit 1; done
	@echo "lint-scripts: OK"

test:
	cargo test --workspace
	python3 -m unittest discover -s bench -p 'test_*.py' -v
