#!/usr/bin/env python3
"""Collect dependency license metadata and bundled notices from locked Cargo sources.

Run cargo fetch first. The default inventories the workspace; --target collects
normal/build dependencies reachable from the service for that release target.
"""

import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Collect service runtime dependencies for this release target")
    args = parser.parse_args()
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"]
    if args.target:
        command += ["--filter-platform", args.target]
    metadata = json.loads(subprocess.check_output(command, cwd=ROOT))
    required = None
    if args.target:
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        service = next(p["id"] for p in metadata["packages"] if p["name"] == "anila-service" and not p["source"])
        required = set()
        pending = [service]
        while pending:
            package = pending.pop()
            if package in required:
                continue
            required.add(package)
            pending.extend(dep["pkg"] for dep in nodes[package]["deps"]
                           if any(kind["kind"] != "dev" for kind in dep["dep_kinds"]))
    packages = sorted((p for p in metadata["packages"] if p["source"]),
                      key=lambda p: (p["name"], p["version"]))
    inventory = ["# Third-party notices", "",
                 "anila is MIT licensed; dependencies retain their own licenses.",
                 "Vendored xterm.js 6.0.0 and addon-fit 0.11.0 retain the MIT notice in",
                 "`frontend/vendor/xterm/LICENSE`. Do not remove it from source distributions.", "",
                 "The table below records Cargo.lock dependency metadata, including development",
                 "and target-specific packages. SPDX alternatives describe upstream options;",
                 "they do not relicense the package. This inventory is not a legal audit.", "",
                 "Release archives include `THIRD_PARTY_LICENSES.txt`, generated from fetched",
                 "crate sources by `python3 scripts/third_party_notices.py`. Regenerate after",
                 "dependency updates. Some crates bundle native code or data with additional",
                 "notices; retain those notices when redistributing modified dependencies.", "",
                 "| Package | Version | Declared license |", "| --- | --- | --- |"]
    notices = ["Third-party dependency notices\n"]
    if args.target:
        notices.append(f"Service runtime target: {args.target}\n")
    missing = []
    for package in packages:
        name, version = package["name"], package["version"]
        inventory.append(f'| {name} | {version} | {package.get("license") or "See bundled license file"} |')
        if required is not None and package["id"] not in required:
            continue
        source = Path(package["manifest_path"]).parent
        files = set()
        for path in source.rglob("*"):
            if path.is_file() and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE")):
                files.add(path)
        if package.get("license_file"):
            files.add(source / package["license_file"])
        if not files:
            supplemental = ROOT / "licenses/dependencies" / f"{name}-{version}"
            if supplemental.exists():
                files.update(path for path in supplemental.iterdir() if path.is_file())
            else:
                missing.append(f"{name} {version}")
        notices.append(f'\n=== {name} {version} ===\nDeclared license: {package.get("license")}\n')
        notices.append("Declared authors: " + ", ".join(package.get("authors", [])) + "\n")
        for path in sorted(files):
            relative = path.relative_to(source) if path.is_relative_to(source) else path.relative_to(ROOT)
            notices.append(f"\n--- {relative} ---\n")
            notices.append(path.read_text(encoding="utf-8", errors="replace"))
    notices.append("\n=== Vendored xterm.js and addon-fit ===\n")
    notices.append((ROOT / "frontend/vendor/xterm/LICENSE").read_text())
    if missing:
        inventory += ["", "## Packages without available full license text", "",
                      "These crates declare licenses in metadata but their downloaded source archives",
                      "omit license files and no pinned upstream supplement is available locally.",
                      "Review their upstream notices before distributing binaries containing them.", ""]
        inventory += [f"- {package}" for package in missing]
        if args.target:
            raise SystemExit("Review runtime packages without license files: " + ", ".join(missing))
        print(f"Review full-text notices for {len(missing)} packages before distributing their binaries.")
    # Keep the committed workspace inventory stable across release targets.
    if not args.target:
        (ROOT / "THIRD_PARTY_NOTICES.md").write_text("\n".join(inventory) + "\n")
    (ROOT / "THIRD_PARTY_LICENSES.txt").write_text("\n".join(notices))
    print(f"Collected license metadata and notices for {len(packages)} Cargo packages and xterm.")


if __name__ == "__main__":
    main()
