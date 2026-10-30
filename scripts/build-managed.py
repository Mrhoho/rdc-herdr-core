#!/usr/bin/env python3
"""Build one locked, signed universal managed distribution (see managed-release.md)."""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import pathlib
import platform
import re
import subprocess
import sys
import tarfile
import tempfile

TARGETS = {"arm64": "aarch64-apple-darwin", "x86_64": "x86_64-apple-darwin"}


def run(args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def output(args, **kwargs):
    return run(args, stdout=subprocess.PIPE, text=True, **kwargs).stdout.strip()


def git(repo, *args):
    return output(["git", "-C", repo, *args])


def validate_identity(commit, patchset, release_seq, config, bundle_id, identity, timestamp):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("commit must be a full lowercase 40-character object ID")
    if not 0 < release_seq < 2**64:
        raise ValueError("release-seq must be a positive u64")
    if not patchset or any(ord(c) < 32 for c in patchset):
        raise ValueError("patchset must be a nonempty printable identifier")
    if config not in ("release", "dev"):
        raise ValueError("config must be release or dev")
    if not re.fullmatch(r"[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+", bundle_id):
        raise ValueError("bundle-id must be an explicit reverse-domain identifier")
    if not identity:
        raise ValueError("signing identity is required; use '-' explicitly for ad-hoc")
    if identity == "-" and timestamp:
        raise ValueError("ad-hoc signing cannot request a trusted timestamp")


def check_tree(repo, commit):
    if pathlib.Path(git(repo, "rev-parse", "--show-toplevel")).resolve() != repo.resolve():
        raise ValueError("source must be the checkout root")
    if git(repo, "rev-parse", "HEAD") != commit:
        raise ValueError("source HEAD does not equal the locked commit")
    if git(repo, "status", "--porcelain", "--untracked-files=all", "--ignore-submodules=none"):
        raise ValueError("source checkout must be clean (including untracked files and submodules)")


def source_trees(repo, commit, relative=pathlib.Path(".")):
    check_tree(repo, commit)
    trees = [(repo, commit, relative)]
    records = git(repo, "ls-tree", "-rz", commit).split("\0")
    for record in records:
        if not record:
            continue
        meta, name = record.split("\t", 1)
        mode, kind, oid = meta.split()
        if mode == "160000":
            child = repo / name
            if not (child / ".git").exists():
                raise ValueError("initialize the pinned submodule: " + str(relative / name))
            trees.extend(source_trees(child, oid, relative / name))
    return trees


def snapshot(trees, destination):
    # Read committed Git objects, never ignored build files or mutable working bytes.
    for repo, commit, relative in trees:
        dest = destination / relative
        dest.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryFile() as archive:
            run(["git", "-C", repo, "archive", "--format=tar", commit], stdout=archive)
            archive.seek(0)
            with tarfile.open(fileobj=archive) as tar:
                tar.extractall(dest)


@contextlib.contextmanager
def cache_lock(target):
    target.mkdir(parents=True, exist_ok=True)
    with (target / ".managed-release.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("target cache is in use by another distribution build") from None
        yield


def verify_build_info(actual, expected):
    if actual != expected:
        raise ValueError("binary build-info does not match the locked distribution identity")


def signing_evidence(details, identity, timestamp):
    fields = dict(line.split("=", 1) for line in details.splitlines() if "=" in line)
    adhoc = fields.get("Signature") == "adhoc"
    runtime = re.search(r"^CodeDirectory .*flags=0x[0-9a-f]+\([^)]*\bruntime\b", details, re.M)
    if adhoc != (identity == "-") or runtime is None:
        raise ValueError("unexpected signing kind or missing hardened runtime")
    if timestamp and "Timestamp" not in fields:
        raise ValueError("requested trusted timestamp is absent")
    return {"kind": "ad-hoc" if adhoc else "certificate", "requestedIdentity": identity,
            "teamIdentifier": fields.get("TeamIdentifier"),
            "authority": [line[10:] for line in details.splitlines() if line.startswith("Authority=")],
            "hardenedRuntime": True, "timestampRequested": timestamp,
            "timestamp": fields.get("Timestamp")}


def main():
    driver_sha256 = hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1])
    parser.add_argument("--commit", required=True)
    parser.add_argument("--patchset", required=True)
    parser.add_argument("--release-seq", required=True, type=int)
    parser.add_argument("--config", required=True, choices=["release", "dev"])
    parser.add_argument("--bundle-id", required=True)
    parser.add_argument("--identity", required=True)
    parser.add_argument("--timestamp", action="store_true")
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--target-dir", type=pathlib.Path, help="optional reusable Cargo cache (not a source tree)")
    parser.add_argument("--deployment-target", default="13.0")
    args = parser.parse_args()
    validate_identity(args.commit, args.patchset, args.release_seq, args.config,
                      args.bundle_id, args.identity, args.timestamp)
    if platform.system() != "Darwin":
        raise ValueError("the managed universal distribution must be built and signed on macOS")
    source, dest = args.source.resolve(), args.output.absolute()
    if dest.exists() or dest.is_symlink():
        raise ValueError("output must not already exist")
    dest = dest.resolve()
    if source == dest or source in dest.parents:
        raise ValueError("output must be outside the source checkout")
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,2}", args.deployment_target):
        raise ValueError("invalid macOS deployment target")
    trees = source_trees(source, args.commit)
    dest.parent.mkdir(parents=True, exist_ok=True)
    expected = dict(commit=args.commit, patchset=args.patchset, releaseSeq=args.release_seq,
                    config=args.config + ":" + args.bundle_id)
    # Do not inherit RUSTFLAGS, alternate linkers, feature flags or build identity.
    keep = ("PATH", "HOME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME", "ZIG", "DEVELOPER_DIR")
    env = {k: os.environ[k] for k in keep if k in os.environ}
    env.update(NEARSHELL_CORE_COMMIT=args.commit, NEARSHELL_CORE_PATCHSET=args.patchset,
               NEARSHELL_CORE_RELEASE_SEQ=str(args.release_seq), NEARSHELL_CORE_CONFIG=args.config,
               NEARSHELL_CORE_BUNDLE_ID=args.bundle_id, MACOSX_DEPLOYMENT_TARGET=args.deployment_target)
    with tempfile.TemporaryDirectory(prefix=".managed-build-", dir=dest.parent) as temp:
        work = pathlib.Path(temp)
        src, product = work / "source", work / "product"
        snapshot(trees, src)
        product.mkdir()
        target = args.target_dir.resolve() if args.target_dir else work / "target"
        if target == source or source in target.parents or target == dest or dest in target.parents:
            raise ValueError("target-dir must be outside source and output")
        with cache_lock(target):
            slices = []
            for arch, triple in TARGETS.items():
                run(["cargo", "build", "--locked", "--release", "--no-default-features", "--config", 'profile.release.strip="none"', "--features", "managed",
                     "-p", "herdr-core", "--bin", "nearshell-hc", "--target", triple, "--target-dir", target],
                    cwd=src, env=env)
                slices.append(target / triple / "release/nearshell-hc")
            binary = product / "nearshell-hc"
            run(["xcrun", "lipo", "-create", *slices, "-output", binary])
            arches = output(["xcrun", "lipo", "-archs", binary]).split()
            if set(arches) != set(TARGETS):
                raise ValueError("universal output must contain exactly arm64 and x86_64")
        run(["/usr/bin/codesign", "--force", "--sign", args.identity, "--identifier", args.bundle_id + ".core",
             "--options", "runtime", "--timestamp" if args.timestamp else "--timestamp=none", binary])
        run(["/usr/bin/codesign", "--verify", "--strict", "--all-architectures", binary])
        details = run(["/usr/bin/codesign", "-d", "--verbose=4", binary],
                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True).stderr
        signing = signing_evidence(details, args.identity, args.timestamp)
        # Verify the host slice by execution. Cross slice gets structural/signature checks;
        # its execution must be checked on Intel or Rosetta by the release contract suite.
        actual = json.loads(output([binary, "build-info", "--json"], env=env))
        verify_build_info(actual, expected)
        version = output([binary, "--version"], env=env)
        for repo, commit, _ in trees:
            check_tree(repo, commit)
        manifest = {"schemaVersion": 1, "binary": binary.name,
                    "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "build": actual,
                    "version": version, "architectures": sorted(arches), "signing": signing,
                    "source": {"commit": args.commit, "submodules": {
                        str(rel): commit for _, commit, rel in trees[1:]}},
                    "buildDriverSha256": driver_sha256,
                    "toolchain": {"rustc": output(["rustc", "--version"], env=env),
                                  "cargo": output(["cargo", "--version"], env=env),
                                  "zig": output([env.get("ZIG", "zig"), "version"], env=env),
                                  "xcode": output(["xcodebuild", "-version"], env=env)},
                    "macOSDeploymentTarget": args.deployment_target,
                    "profile": {"name": "release", "strip": "none", "features": ["managed"]},
                    "executedArchitectures": ["arm64" if platform.machine() == "arm64" else "x86_64"]}
        (product / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        product.rename(dest)
    print(dest / "manifest.json")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit("build-managed: " + str(error))
