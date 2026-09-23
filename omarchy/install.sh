#!/bin/bash
# Install the agent-review launcher (lgtm) for Omarchy. Safe to re-run.
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$dir/.." && pwd)"

# The launcher builds and runs lgtm from this checkout, so bake its path in.
mkdir -p "$HOME/.local/bin"
sed "s#__LGTM_REPO__#$repo#" "$dir/agent-review" > "$HOME/.local/bin/agent-review"
chmod 755 "$HOME/.local/bin/agent-review"

# The desktop-entry basename matches the app_id lgtm's window reports
# ("agent-review"), which is how the compositor resolves the window's icon.
install -Dm644 "$dir/agent-review.desktop" "$HOME/.local/share/applications/agent-review.desktop"
install -Dm644 "$dir/agent-review.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/agent-review.svg"

echo "agent-review installed — press Super+Space and search 'agent-review'."
echo "The first launch builds lgtm in release mode, which takes a few minutes."
