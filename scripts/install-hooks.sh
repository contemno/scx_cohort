#!/bin/bash
# Install git hooks for this project.
# Usage: ./scripts/install-hooks.sh
#
# The pre-push hook is intentionally FAST (lint/format/syntax only, ~seconds) so
# nobody is tempted to disable it. Full clippy + tests run in CI on PRs (clippy
# needs a full BPF/libbpf build, which is minutes, not seconds).

set -euo pipefail

HOOKS_DIR="$(git rev-parse --git-dir)/hooks"

cat > "${HOOKS_DIR}/pre-push" << 'HOOK'
#!/bin/bash
# Pre-push hook: fast lint + syntax check. Full clippy/tests run in CI.
# The checked-file lists live in the Makefile (make lint-fast), the same
# entry point CI uses — keep them in one place.
set -euo pipefail

if command -v make >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1; then
    make lint-fast
else
    echo "pre-push: make/cargo not found; skipping fast lint" >&2
fi

echo "pre-push: OK"
HOOK

chmod +x "${HOOKS_DIR}/pre-push"
echo "Installed pre-push hook to ${HOOKS_DIR}/pre-push"
