"""Release driver guards; no compiler, signing key or running core needed."""
import importlib.util
import pathlib
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("release", pathlib.Path(__file__).with_name("build-managed.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseGuards(unittest.TestCase):
    def test_identity_requires_full_commit_positive_sequence_and_safe_bundle(self):
        valid = dict(commit="a" * 40, patchset="P1-P4,P6,P8-P9", release_seq=1,
                     config="release", bundle_id="com.nearshell.agent", identity="-", timestamp=False)
        release.validate_identity(**valid)
        for key, value in [("commit", "HEAD"), ("commit", "a" * 39), ("release_seq", 0),
                           ("release_seq", 2**64), ("config", "debug"), ("patchset", ""),
                           ("bundle_id", "../other"), ("bundle_id", "a..b"), ("identity", ""),
                           ("timestamp", True)]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                release.validate_identity(**(valid | {key: value}))

    def test_clean_guard_rejects_dirty_tree_and_wrong_commit(self):
        source = pathlib.Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as temp:
            repo = pathlib.Path(temp) / "repo"
            subprocess.run(["git", "clone", "--quiet", "--shared", "--no-checkout", str(source), str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "checkout", "--quiet", "--detach", "HEAD"], check=True)
            commit = release.git(repo, "rev-parse", "HEAD").strip()
            release.check_tree(repo, commit)
            with self.assertRaisesRegex(ValueError, "initialize the pinned submodule"):
                release.source_trees(repo, commit)
            with self.assertRaises(ValueError):
                release.check_tree(repo, "0" * 40)
            (repo / "untracked").write_text("dirty")
            with self.assertRaises(ValueError):
                release.check_tree(repo, commit)
            (repo / "untracked").unlink()
            (repo / ".git/info/exclude").write_text("ignored-injection\n")
            (repo / "ignored-injection").write_text("must not enter the build")
            release.check_tree(repo, commit)
            exported = pathlib.Path(temp) / "exported"
            release.snapshot([(repo, commit, pathlib.Path("."))], exported)
            self.assertFalse((exported / "ignored-injection").exists())
            self.assertEqual((exported / "Cargo.toml").read_text().strip(),
                             release.git(repo, "show", commit + ":Cargo.toml"))
            with (repo / "Cargo.toml").open("a") as f:
                f.write("\n# dirty\n")
            with self.assertRaises(ValueError):
                release.check_tree(repo, commit)

    def test_cache_rejects_concurrent_distribution_builds(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = pathlib.Path(temp)
            with release.cache_lock(cache):
                with self.assertRaisesRegex(ValueError, "cache is in use"):
                    with release.cache_lock(cache):
                        self.fail("concurrent lock acquired")
            with release.cache_lock(cache):
                pass

    def test_signature_evidence_is_observed_and_runtime_required(self):
        adhoc = "Signature=adhoc\nTeamIdentifier=not set\nCodeDirectory flags=0x10002(adhoc,runtime)"
        self.assertEqual(release.signing_evidence(adhoc, "-", False)["kind"], "ad-hoc")
        cert = "Authority=Developer ID Application: Test\nTeamIdentifier=TEAM\nCodeDirectory flags=0x10000(runtime)\nTimestamp=Oct 30, 2026"
        self.assertEqual(release.signing_evidence(cert, "Test", True)["teamIdentifier"], "TEAM")
        for evidence, identity, timestamp in [(adhoc, "Test", False), (cert, "-", False),
                                               (adhoc, "-", True), ("Signature=adhoc", "-", False)]:
            with self.assertRaises(ValueError):
                release.signing_evidence(evidence, identity, timestamp)

    def test_build_identity_must_equal_injected_values(self):
        expected = dict(commit="a" * 40, patchset="a4", releaseSeq=2, config="dev:com.example.dev")
        release.verify_build_info(expected, expected)
        for key in expected:
            with self.subTest(key=key), self.assertRaises(ValueError):
                release.verify_build_info(expected | {key: "wrong"}, expected)


if __name__ == "__main__":
    unittest.main()
