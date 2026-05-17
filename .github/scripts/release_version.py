#!/usr/bin/env python3
"""Whether this commit is a release, and of what: Cargo.toml's version.

release.yml asks this first. The version in Cargo.toml is the release: a
commit whose version no tag names yet is released as that version, and the
tag is created with the GitHub release. A version a tag already names is
not released again, so any other push that changes Cargo.toml stops here.

The version has to come after every tag already there, by SemVer
precedence, or this fails the run. An older version released after a newer
one would become the repository's latest release, and `releases/latest`,
and `tw update` with it, would go backwards.

    release_version.py <checkout>                 decide, as above
    release_version.py --on-crates-io <checkout>  is that version on crates.io?

Writes its answers to $GITHUB_OUTPUT when that is set, and prints them.
"""

import json
import os
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request

SEMVER = re.compile(r"(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?")


def precedence(version):
    """A sort key in SemVer precedence order, or None for a tag that is not
    a version at all."""
    m = SEMVER.fullmatch(version)
    if not m:
        return None
    core = tuple(int(n) for n in m.group(1, 2, 3))
    if m.group(4) is None:
        # A release comes after every pre-release of the same version.
        return (core, (1,))
    ids = tuple((0, int(i), "") if i.isdigit() else (1, 0, i) for i in m.group(4).split("."))
    return (core, (0,) + ids)


def output(**answers):
    lines = "".join(f"{k}={v}\n" for k, v in answers.items())
    print(lines, end="")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as f:
            f.write(lines)


def fail(message):
    print(f"::error::{message}", file=sys.stderr)
    sys.exit(1)


def package(checkout):
    with open(os.path.join(checkout, "Cargo.toml"), "rb") as f:
        manifest = tomllib.load(f)["package"]
    return manifest["name"], manifest["version"]


def tags(checkout):
    listing = subprocess.run(
        ["git", "-C", checkout, "ls-remote", "--tags", "origin"],
        check=True, capture_output=True, text=True,
    ).stdout
    return {
        line.split("refs/tags/", 1)[1].removesuffix("^{}")
        for line in listing.splitlines()
        if "refs/tags/" in line
    }


def decide(checkout):
    crate, version = package(checkout)
    if precedence(version) is None:
        fail(f"Cargo.toml's version {version!r} is not a SemVer version")
    existing = tags(checkout)
    if version in existing:
        print(f"{version} is tagged already: nothing to release.")
        output(crate=crate, version=version, release="false", prerelease="false")
        return
    newer = sorted((t for t in existing if precedence(t) and precedence(t) > precedence(version)), key=precedence)
    if newer:
        fail(
            f"Cargo.toml says {version}, but {newer[-1]} is tagged already. "
            f"Set a version after {newer[-1]} to release."
        )
    output(crate=crate, version=version, release="true", prerelease=str("-" in version).lower())


def on_crates_io(checkout):
    """Read the crates.io index, so a run that published the crate and then
    failed later can be re-run without publishing it twice."""
    crate, version = package(checkout)
    name = crate.lower()
    path = {1: f"1/{name}", 2: f"2/{name}", 3: f"3/{name[0]}/{name}"}.get(len(name), f"{name[:2]}/{name[2:4]}/{name}")
    try:
        with urllib.request.urlopen(f"https://index.crates.io/{path}", timeout=30) as index:
            published = any(json.loads(line)["vers"] == version for line in index.read().decode().splitlines() if line)
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        published = False
    print(f"{crate} {version} is {'on' if published else 'not on'} crates.io.")
    output(published=str(published).lower())


if __name__ == "__main__":
    args = sys.argv[1:]
    if args[:1] == ["--on-crates-io"]:
        on_crates_io(args[1] if len(args) > 1 else ".")
    else:
        decide(args[0] if args else ".")
