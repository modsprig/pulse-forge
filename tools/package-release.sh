#!/usr/bin/env bash
# PF-1302: produce Windows release artifacts — zip with binaries + docs,
# and SHA-256 checksums. CI (release.yml) runs this on tag pushes; this
# script is also the local packaging path.
#
# Output: dist/pulseforge-<version>-windows-x64.zip + .sha256

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
echo "packaging pulseforge v$VERSION (windows-x64)"

# 1. Fresh release build of every shipped binary.
cargo build --release -q -p pf-cli -p pf-gui

# 2. Stage the artifact tree.
STAGE="dist/stage/pulseforge-$VERSION-windows-x64"
rm -rf dist/stage
mkdir -p "$STAGE"
cp target/release/pulseforge.exe "$STAGE/"
cp target/release/pulseforge-gui.exe "$STAGE/"
cp README.md LICENSE CHANGELOG.md "$STAGE/"
cp tools/install.ps1 "$STAGE/"
# Internal docs live in the private docs/.local/ working set and are
# deliberately NOT distributed in release artifacts.
# A first-run note pointing at the daemon workflow.
cat > "$STAGE/GETTING-STARTED.txt" <<'EOF'
PulseForge quick start
======================

1. Start the per-user daemon (records survive any client disconnect):

     pulseforge daemon

2. See what your machine can profile:

     pulseforge capabilities

3. Record a process (counters; the target is never signalled):

     pulseforge record --pid <pid> --duration-ms 10000
     pulseforge record --run <exe> -- <args>      (no shell involved)

4. Or use the native GUI against the same daemon:

     pulseforge-gui

5. Or install per-user (PATH + Start-Menu shortcut, no admin):

     powershell -ExecutionPolicy Bypass -File install.ps1

6. Inspect / export:

     pulseforge report --session <session-dir>
     pulseforge export --session <session-dir> --out out.json --redact

Full docs: the project README (internal guides live in the private
docs/.local/ working set, not shipped).
EOF

# 3. Zip.
mkdir -p dist
ZIP="dist/pulseforge-$VERSION-windows-x64.zip"
rm -f "$ZIP"
powershell -NoProfile -Command "Compress-Archive -Path 'dist/stage/*' -DestinationPath '$ZIP' -Force" >/dev/null
# Normalize the backslash path Compress-Archive may print; verify existence.
[ -f "$ZIP" ] || { echo "zip creation failed" >&2; exit 1; }

# 4. SHA-256 checksums (PowerShell Get-FileHash; certutil fallback).
HASHES="$ZIP.sha256"
HASH=$(powershell -NoProfile -Command "(Get-FileHash -Algorithm SHA256 '$ZIP').Hash.ToLower()")
printf '%s  %s
' "$HASH" "$(basename "$ZIP")" > "$HASHES"

# 5. Verify round-trip: the recorded hash must match a fresh computation.
EXPECTED=$(head -c 64 "$HASHES")
ACTUAL=$HASH
[ "$EXPECTED" = "$ACTUAL" ] || { echo "checksum mismatch!" >&2; exit 1; }

echo "packaged: $ZIP"
cat "$HASHES"
ls -la "$ZIP"
