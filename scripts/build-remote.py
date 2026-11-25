#!/usr/bin/env python3
"""Build one locked remote nhc for Linux (see docs/nearshell-build.md): a static
musl binary and its manifest entry, cross-compiled on macOS or native on Linux."""
import argparse
import hashlib
import importlib.util
import json
import os
import pathlib
import platform
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
# Same clean-snapshot discipline and identity rules as the managed driver.
_spec = importlib.util.spec_from_file_location("build_managed", HERE / "build-managed.py")
managed = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(managed)

TARGETS = {"x86_64-unknown-linux-musl": "x86_64", "aarch64-unknown-linux-musl": "aarch64"}
# Where crates.io sources appear in panic locations, whichever machine built it.
# The checkout needs no remap: Cargo names workspace files relative to it.
CARGO = "/nearshell/cargo"


def expected_build(commit, patchset, release_seq, config, bundle_id):
    return dict(commit=commit, patchset=patchset, releaseSeq=release_seq, config=f"{config}:{bundle_id}:remote")


def cargo_command(triple, target_dir, cargo_home):
    """rustc's own rust-lld (no extra toolchain), a GNU build id as CI's native
    links have, and Cargo's home remapped: on one kind of host the bytes do not
    depend on the folder they were built in (a macOS cross build and a native
    Linux build still differ: rust-lld's own comment and string order). Stable
    flags keep a cache."""
    flags = ["-C", "link-arg=--build-id=sha1", f"--remap-path-prefix={cargo_home}={CARGO}"]
    return ["cargo", "build", "--locked", "--release", "--no-default-features", "--features", "nearshell",
            "-p", "herdr-core", "--bin", "nhc", "--target", triple, "--target-dir", str(target_dir),
            "--config", f'target.{triple}.linker="rust-lld"',
            "--config", f"target.{triple}.rustflags={json.dumps(flags)}"]


def runs_here(triple):
    machine = {"amd64": "x86_64", "arm64": "aarch64"}.get(platform.machine().lower(), platform.machine().lower())
    return platform.system() == "Linux" and TARGETS[triple] == machine


def entry(binary, triple, build):
    data = binary.read_bytes()
    return {"target": triple, "file": binary.name, "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(), "build": build}


def main():
    drivers = {name: hashlib.sha256((HERE / name).read_bytes()).hexdigest()
               for name in ("build-remote.py", "build-managed.py")}
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=pathlib.Path, default=HERE.parent)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--patchset", required=True)
    parser.add_argument("--release-seq", required=True, type=int)
    parser.add_argument("--config", required=True, choices=["release", "dev"])
    parser.add_argument("--bundle-id", required=True)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--target-dir", type=pathlib.Path, help="optional reusable Cargo cache (not a source tree)")
    args = parser.parse_args()
    managed.validate_build(args.commit, args.patchset, args.release_seq, args.config, args.bundle_id)
    if platform.system() not in ("Darwin", "Linux"):
        raise ValueError("the remote build is made on macOS (cross) or Linux")
    source, dest = args.source.resolve(), args.output.absolute()
    if dest.exists() or dest.is_symlink():
        raise ValueError("output must not already exist")
    dest = dest.resolve()
    if source == dest or source in dest.parents:
        raise ValueError("output must be outside the source checkout")
    trees = managed.source_trees(source, args.commit)
    dest.parent.mkdir(parents=True, exist_ok=True)
    expected = expected_build(args.commit, args.patchset, args.release_seq, args.config, args.bundle_id)
    # Do not inherit RUSTFLAGS, alternate linkers, feature flags or build identity.
    keep = ("PATH", "HOME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "ZIG")
    env = {k: os.environ[k] for k in keep if k in os.environ}
    env.update(NEARSHELL_CORE_COMMIT=args.commit, NEARSHELL_CORE_PATCHSET=args.patchset,
               NEARSHELL_CORE_RELEASE_SEQ=str(args.release_seq), NEARSHELL_CORE_CONFIG=args.config,
               NEARSHELL_CORE_BUNDLE_ID=args.bundle_id)
    cargo_home = pathlib.Path(env.get("CARGO_HOME") or pathlib.Path.home() / ".cargo").resolve()
    with tempfile.TemporaryDirectory(prefix=".remote-build-", dir=dest.parent) as temp:
        work = pathlib.Path(temp)
        src, product = work / "source", work / "product"
        managed.snapshot(trees, src)
        product.mkdir()
        target = args.target_dir.resolve() if args.target_dir else work / "target"
        if target == source or source in target.parents or target == dest or dest in target.parents:
            raise ValueError("target-dir must be outside source and output")
        with managed.cache_lock(target):
            managed.run(cargo_command(args.target, target, cargo_home), cwd=src, env=env)
            binary = product / f"nhc-{args.target}"
            binary.write_bytes((target / args.target / "release/nhc").read_bytes())
            binary.chmod(0o755)
        # A cross build cannot run here: the identity is the one it was built
        # with, and the installing side checks what the binary itself reports.
        executed = runs_here(args.target)
        if executed:
            managed.verify_build_info(json.loads(managed.output([binary, "build-info", "--json"], env=env)), expected)
        for repo, commit, _ in trees:
            managed.check_tree(repo, commit)
        manifest = {"schemaVersion": 1, "entry": entry(binary, args.target, expected), "executed": executed,
                    "source": {"commit": args.commit, "submodules": {
                        str(rel): commit for _, commit, rel in trees[1:]}},
                    "buildDriverSha256": drivers,
                    "toolchain": {"rustc": managed.output(["rustc", "--version"], env=env),
                                  "cargo": managed.output(["cargo", "--version"], env=env),
                                  "zig": managed.output([env.get("ZIG", "zig"), "version"], env=env),
                                  "host": f"{platform.system()} {platform.machine()}"},
                    "profile": {"name": "release", "strip": "symbols", "features": ["nearshell"],
                                "linker": "rust-lld", "buildId": "sha1", "remap": [CARGO]}}
        (product / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        product.rename(dest)
    print(dest / "manifest.json")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit("build-remote: " + str(error))
