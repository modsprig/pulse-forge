#!/usr/bin/env python3
"""PF-108 local license policy check (mirror of the deny.toml allow-list).

cargo-deny runs in CI; this script gives the same verdict offline by
walking `cargo metadata` and evaluating each license expression against
the allow-list (parentheses stripped, OR/AND leaves all required).
"""
import json, re, subprocess, sys

ALLOWED = {
    "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause",
    "BSD-3-Clause", "ISC", "Unicode-3.0", "Zlib", "CC0-1.0",
    "CDLA-Permissive-2.0", "0BSD", "Unlicense", "BSL-1.0", "OFL-1.1",
    "LicenseRef-UFL-1.0", "LGPL-2.1-or-later",
}

def leaves(expr: str):
    expr = expr.replace("(", " ").replace(")", " ")
    return [t.strip() for t in re.split(r"\s+(?:OR|AND)\s+|/", expr) if t.strip()]

def main() -> int:
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1"],
        capture_output=True, text=True).stdout)
    bad = {}
    for pkg in meta["packages"]:
        lic = pkg.get("license") or ""
        bad_leaves = [l for l in leaves(lic) if l not in ALLOWED]
        if bad_leaves:
            bad.setdefault(pkg["name"], []).append((lic, bad_leaves))
    if bad:
        print("NON-ALLOWED LICENSES:")
        for name, entries in sorted(bad.items()):
            for lic, missing in entries:
                print(f"  {name}: {lic} (disallowed: {missing})")
        return 1
    print(f"license check: all {len(meta['packages'])} packages within policy")
    return 0

if __name__ == "__main__":
    sys.exit(main())
