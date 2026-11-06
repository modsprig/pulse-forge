#!/usr/bin/env bash
# PF-1301: produce the Linux .deb installer — the repeatable recipe
# (the 0.5.0 deb was hand-built; this script replaces that).
#
# Run on Linux (or WSL2) from the repo root. Output:
#   dist/pulseforge_<version>_amd64.deb
#
# The package ships the CLI + GUI binaries and docs. Install/verify/remove
# is the PF-1301 evidence cycle:
#   dpkg -i dist/pulseforge_<v>_amd64.deb
#   pulseforge version && pulseforge record --run <fixture> ...
#   pulseforge stacks --session <dir> --limit 4
#   dpkg -r pulseforge

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
echo "building pulseforge_${VERSION}_amd64.deb"

# 1. Release build of the shipped binaries.
cargo build --release -q -p pf-cli -p pf-gui

# 2. Stage the package tree (FHS layout; arch-independent data in /usr/share).
STAGE="dist/deb-stage/pulseforge-$VERSION"
rm -rf dist/deb-stage
DEB_DIR="$STAGE/DEBIAN"
mkdir -p "$DEB_DIR" "$STAGE/usr/bin" "$STAGE/usr/share/doc/pulseforge"
cp target/release/pulseforge "$STAGE/usr/bin/"
cp target/release/pulseforge-gui "$STAGE/usr/bin/"
cp README.md LICENSE "$STAGE/usr/share/doc/pulseforge/"
gzip -9n -c CHANGELOG.md > "$STAGE/usr/share/doc/pulseforge/changelog.gz"

cat > "$DEB_DIR/control" <<EOF
Package: pulseforge
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Maintainer: PulseForge maintainers <pulseforge@localhost>
Description: local-first Rust process profiler
 Records what a process actually consumes — CPU time, memory residency,
 I/O — with per-thread perf_event_open stack sampling, DWARF
 symbolization, bounded flamegraphs, speedscope/pprof exports, a native
 egui UI and an MCP interface for coding agents. Unprivileged per-user
 daemon; the kernel's security policy is never changed.
EOF

cat > "$DEB_DIR/postinst" <<'EOF'
#!/bin/sh
set -e
# No system state is mutated beyond the package files themselves.
echo "pulseforge installed — run 'pulseforge capabilities' to probe this machine"
EOF
chmod 755 "$DEB_DIR/postinst"

# 3. dpkg-deb refuses the 777 that drvfs mounts report, and chmod does
# not stick there — build from a copy on the native filesystem.
NATIVE=$(mktemp -d)
cp -r "$STAGE" "$NATIVE/pkg"
chmod -Rf go-w "$NATIVE/pkg"
mkdir -p dist
dpkg-deb --build --root-owner-group "$NATIVE/pkg" "dist/pulseforge_${VERSION}_amd64.deb"
rm -rf "$NATIVE"
dpkg-deb --info "dist/pulseforge_${VERSION}_amd64.deb" | head -8
ls -la "dist/pulseforge_${VERSION}_amd64.deb"
