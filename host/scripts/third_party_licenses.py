#!/usr/bin/env python3
"""THIRD-PARTY-LICENSES.txt for a release: the licenses of what the traffic-police binary contains
besides its own code (Apache-2.0, see LICENSE). That is the Rust crates it links on the released
platforms, with the license files each crate ships, and the attach-mode agent's third-party code
(slicer, and the JVMTI header), whose texts are in android/attach-agent/third_party.

It asks cargo, so the crates' sources must be downloadable (they are once the host has been
built). The release workflow attaches the output to each release:

    python3 host/scripts/third_party_licenses.py > THIRD-PARTY-LICENSES.txt
"""

import hashlib
import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[2]
HOST = ROOT / "host"
AGENT = ROOT / "android/attach-agent/third_party"
# the files a crate ships its license in: LICENSE, LICENSE-MIT, COPYING, NOTICE, ...
LICENSE_FILES = ("license", "licence", "copying", "notice", "unlicense", "copyright")
RULE = "=" * 100


def cargo(*args):
    return subprocess.run(["cargo", *args], cwd=HOST, check=True, capture_output=True, text=True).stdout


def linked():
    """(name, version) of every crate linked into the binary on any platform: normal
    dependencies, without proc macros (they run at build time and are not in the binary)."""
    out = cargo("tree", "--locked", "-p", "traffic-police", "-e", "normal", "--target", "all",
                "--prefix", "none", "--format", "{p}")
    crates = set()
    for line in out.splitlines():
        line = line.removesuffix(" (*)").strip()
        if line and "(proc-macro)" not in line:
            name, version = line.split()[:2]
            crates.add((name, version.removeprefix("v")))
    return crates


def license_texts(package):
    folder = pathlib.Path(package["manifest_path"]).parent
    files = sorted(p for p in folder.iterdir() if p.is_file() and p.name.lower().startswith(LICENSE_FILES))
    return [(p.name, p.read_text(encoding="utf-8", errors="replace").strip()) for p in files]


def main():
    meta = json.loads(cargo("metadata", "--locked", "--format-version", "1"))
    ours = set(meta["workspace_members"])
    packages = {(p["name"], p["version"]): p for p in meta["packages"] if p["id"] not in ours}
    crates = sorted((packages[c] for c in linked() if c in packages), key=lambda p: (p["name"], p["version"]))

    print("traffic-police is licensed under the Apache License, Version 2.0 (LICENSE).")
    print()
    print("The binary also contains the third-party software below, under the licenses that follow.")
    print()
    print("1. The attach-mode agent, built into the binary")
    print()
    print("   slicer (Android Open Source Project, platform/tools/dexter)    Apache-2.0")
    print("   jvmti.h (OpenJDK, as shipped in Android's ART; declarations    GPL-2.0-only WITH Classpath-exception-2.0")
    print("   only: the agent calls the JVMTI implementation of the device)")
    print()
    print(f"2. Rust crates ({len(crates)})")
    print()
    width = max(len(f"{p['name']} {p['version']}") for p in crates)
    for p in crates:
        print(f"   {p['name'] + ' ' + p['version']:<{width}}  {p.get('license') or '(no license field)'}")

    # each distinct text once, with the crates that ship it
    texts = {}
    for p in crates:
        found = license_texts(p)
        if not found:
            found = [("", f"(the crate ships no license file; its license is {p.get('license')})")]
        for name, text in found:
            key = hashlib.sha256("\n".join(l.rstrip() for l in text.splitlines()).encode()).hexdigest()
            texts.setdefault(key, (text, []))[1].append(f"{p['name']} {p['version']}" + (f" ({name})" if name else ""))

    print()
    print(RULE)
    print("slicer: Apache License 2.0")
    print(RULE)
    print((AGENT / "SLICER_LICENSE").read_text(encoding="utf-8").strip())
    print()
    print(RULE)
    print("jvmti.h: GPL-2.0 with the Classpath exception")
    print(RULE)
    for f in ("JVMTI_HEADER_NOTICE", "OPENJDK_ASSEMBLY_EXCEPTION.txt", "GPL-2.0.txt"):
        print((AGENT / f).read_text(encoding="utf-8").strip())
        print()
    for text, users in texts.values():
        print(RULE)
        print("\n".join(users))
        print(RULE)
        print(text)
        print()


if __name__ == "__main__":
    main()
