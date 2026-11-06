#!/usr/bin/env python3
"""Generate a CycloneDX-style SBOM for the release artifacts (PF-1210).

The SBOM lists every locked crate with version and license, derived from
`cargo metadata` (the authoritative resolution the release binary was
built from). Written to dist/SBOM.json next to the other release
artifacts; release.yml attaches it to the GitHub release.
"""
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def main() -> int:
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1"],
            capture_output=True,
            text=True,
            cwd=REPO,
        ).stdout
    )
    components = []
    for pkg in meta["packages"]:
        components.append(
            {
                "type": "library",
                "name": pkg["name"],
                "version": pkg["version"],
                "purl": f"pkg:cargo/{pkg['name']}@{pkg['version']}",
                "licenses": [
                    {"expression": lic}
                    for lic in [pkg.get("license") or "unknown"]
                ],
                "description": (pkg.get("description") or "")[:200],
            }
        )
    root = meta["resolve"]["root"]
    sbom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.4",
        "serialNumber": f"urn:uuid:pf-{int(datetime.now(timezone.utc).timestamp())}",
        "metadata": {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "tools": [{"name": "tools/sbom.py", "vendor": "PulseForge"}],
            "component": {
                "type": "application",
                "name": "pulseforge",
                "version": next(
                    p["version"]
                    for p in meta["packages"]
                    if p["name"] == "pf-cli"
                ),
            },
        },
        "components": components,
        "_note": f"root package: {root}; generated from cargo metadata (Cargo.lock)",
    }

    out = REPO / "dist" / "SBOM.json"
    out.parent.mkdir(exist_ok=True)
    out.write_text(json.dumps(sbom, indent=2) + "\n")
    print(f"SBOM: {out} ({len(components)} components)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
