#!/usr/bin/env bash
# Download the ReplayWebPage JS assets (ui.js, sw.js) into static/replay/.
#
# Assets are PINNED to a specific `replaywebpage` npm release so builds are
# reproducible and we only move to a new ReplayWeb.page version deliberately.
#
# To upgrade:
#   1. Pick a new version from https://www.npmjs.com/package/replaywebpage
#   2. Bump VERSION below (or pass it as an argument)
#   3. Re-run this script and rebuild:  cargo build --release
#   4. Re-test replay in a browser before committing the new assets
#
# Usage:
#   ./scripts/fetch-replay.sh          # fetch the pinned VERSION
#   ./scripts/fetch-replay.sh 2.4.6    # fetch a specific version (one-off)
set -euo pipefail

# Pinned ReplayWeb.page (replaywebpage npm package) version.
VERSION="${1:-2.4.6}"

DEST="$(cd "$(dirname "$0")/.." && pwd)/crates/rustyweb-lib/static/replay"
BASE="https://cdn.jsdelivr.net/npm/replaywebpage@${VERSION}"

mkdir -p "$DEST"

echo "Fetching ReplayWebPage ${VERSION} from ${BASE}..."
# -L: jsDelivr issues redirects; -f: fail (non-zero) on HTTP errors.
curl -fL --progress-bar -o "$DEST/ui.js" "${BASE}/ui.js"
curl -fL --progress-bar -o "$DEST/sw.js" "${BASE}/sw.js"

UI_SIZE=$(wc -c < "$DEST/ui.js" | tr -d ' ')
SW_SIZE=$(wc -c < "$DEST/sw.js" | tr -d ' ')
echo "Done. Assets written to $DEST/"
echo "  ui.js  ${UI_SIZE} bytes"
echo "  sw.js  ${SW_SIZE} bytes"
echo ""
echo "Rebuild the binary to embed the updated assets:"
echo "  cargo build --release"
