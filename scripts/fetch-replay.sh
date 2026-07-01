#!/usr/bin/env bash
# Download ReplayWebPage JS assets into static/replay/.
# Assets are served directly from https://replayweb.page/ (GitHub Pages).
# Usage:
#   ./scripts/fetch-replay.sh        # download current live assets
set -euo pipefail

DEST="$(cd "$(dirname "$0")/.." && pwd)/crates/rustyweb-lib/static/replay"
BASE="https://replayweb.page"

mkdir -p "$DEST"

echo "Fetching ReplayWebPage assets from ${BASE}..."
curl -f --progress-bar -o "$DEST/ui.js" "${BASE}/ui.js"
curl -f --progress-bar -o "$DEST/sw.js" "${BASE}/sw.js"

UI_SIZE=$(wc -c < "$DEST/ui.js" | tr -d ' ')
SW_SIZE=$(wc -c < "$DEST/sw.js" | tr -d ' ')
echo "Done. Assets written to $DEST/"
echo "  ui.js  ${UI_SIZE} bytes"
echo "  sw.js  ${SW_SIZE} bytes"
echo ""
echo "Rebuild the binary to embed the updated assets:"
echo "  cargo build --release"
