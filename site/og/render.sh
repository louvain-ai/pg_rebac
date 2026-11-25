#!/usr/bin/env bash
# Renders og/og.html to public/og.png and og/icon.html to the raster icons, with headless Chromium through
# Playwright's CLI (not a site dependency). Run from site/ after `npm ci`: og/render.sh
set -euo pipefail
cd "$(dirname "$0")/.."
pw() { npx --yes playwright@1.57.0 "$@"; }
here="file://$PWD/og"
pw screenshot --viewport-size=1200,630 --wait-for-timeout=300 "$here/og.html" public/og.png
pw screenshot --viewport-size=180,180 "$here/icon.html#touch" public/apple-touch-icon.png
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
pw screenshot --viewport-size=32,32 "$here/icon.html#ico" "$tmp/32.png"
# An .ico holding one PNG: a 6-byte header, one 16-byte directory entry, then the PNG.
node -e '
  const fs = require("fs"); const png = fs.readFileSync(process.argv[1]);
  const h = Buffer.alloc(22); h.writeUInt16LE(0, 0); h.writeUInt16LE(1, 2); h.writeUInt16LE(1, 4);
  h.writeUInt8(32, 6); h.writeUInt8(32, 7); h.writeUInt16LE(1, 10); h.writeUInt16LE(32, 12);
  h.writeUInt32LE(png.length, 14); h.writeUInt32LE(22, 18);
  fs.writeFileSync("public/favicon.ico", Buffer.concat([h, png]));' "$tmp/32.png"
