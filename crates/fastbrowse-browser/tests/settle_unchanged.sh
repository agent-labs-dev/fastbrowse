#!/usr/bin/env bash
# A portable Chrome keeps this regression runnable in the merge queue's fresh checkout.
set -euo pipefail
review_setup=$(mktemp -d /tmp/fastbrowse-settle-test.XXXXXX)
trap 'rm -rf "$review_setup"' EXIT
npm install --prefix "$review_setup" --no-audit --no-fund @sparticuz/chromium@141.0.0
node - "$review_setup" <<'JS'
const fs = require('node:fs');
const zlib = require('node:zlib');
const path = require('node:path');
const root = process.argv[2];
const bin = path.join(root, 'node_modules/@sparticuz/chromium/bin');
for (const name of ['chromium', 'al2023.tar', 'swiftshader.tar', 'fonts.tar']) {
  fs.writeFileSync(path.join(root, name), zlib.brotliDecompressSync(fs.readFileSync(path.join(bin, name + '.br'))));
}
fs.chmodSync(path.join(root, 'chromium'), 0o755);
JS
for archive in al2023 swiftshader fonts; do
  tar -xf "$review_setup/$archive.tar" -C "$review_setup"
done
mkdir "$review_setup/bin"
cat > "$review_setup/bin/chromium" <<WRAPPER
#!/bin/sh
exec "$review_setup/chromium" --no-sandbox --no-zygote --use-gl=angle --use-angle=swiftshader --enable-unsafe-swiftshader "\$@"
WRAPPER
chmod +x "$review_setup/bin/chromium"
export PATH="$review_setup/bin:$PATH"
export LD_LIBRARY_PATH="$review_setup/lib:${LD_LIBRARY_PATH:-}"
export FONTCONFIG_PATH="$review_setup/fonts"
if ! command -v cargo >/dev/null; then
  for review_cargo in /usr/local/rustup/toolchains/*/bin/cargo; do
    if [ -x "$review_cargo" ]; then
      export PATH="${review_cargo%/cargo}:$PATH"
      break
    fi
  done
fi
cargo test -p fastbrowse-browser --test settle_unchanged -- --nocapture
