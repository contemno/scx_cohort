#!/bin/bash
# Install git hooks for this project.
# Usage: ./scripts/install-hooks.sh
#
# The pre-push hook is intentionally FAST (format check only, ~seconds) so
# nobody is tempted to disable it. Clippy and the full test suite run in CI.

set -euo pipefail

HOOKS_DIR="$(git rev-parse --git-dir)/hooks"

cat > "${HOOKS_DIR}/pre-push" << 'HOOK'
#!/bin/bash
# Pre-push hook: fast format check. Clippy + full tests run in CI.
set -euo pipefail

if command -v cargo >/dev/null 2>&1; then
    cargo fmt --all --check
else
    echo "pre-push: cargo not found; skipping format check"
fi

echo "pre-push: OK"
HOOK

chmod +x "${HOOKS_DIR}/pre-push"
echo "Installed pre-push hook to ${HOOKS_DIR}/pre-push"
