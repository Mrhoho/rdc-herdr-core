"""Remote build driver guards; no compiler or running core needed."""
import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("remote", pathlib.Path(__file__).with_name("build-remote.py"))
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)


class RemoteGuards(unittest.TestCase):
    def test_identity_is_the_managed_rules_with_the_remote_suffix(self):
        self.assertEqual(remote.expected_build("a" * 40, "p", 3, "dev", "com.example.dev"),
                         dict(commit="a" * 40, patchset="p", releaseSeq=3, config="dev:com.example.dev:remote"))
        remote.managed.validate_build("a" * 40, "p", 1, "release", "com.nearshell.agent")
        for bad in [("HEAD", "p", 1, "dev", "a.b"), ("a" * 40, "p", 0, "dev", "a.b"),
                    ("a" * 40, "p", 1, "debug", "a.b"), ("a" * 40, "p", 1, "dev", "../x")]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                remote.managed.validate_build(*bad)

    def test_cargo_links_with_rust_lld_a_build_id_and_no_host_paths(self):
        for triple in remote.TARGETS:
            command = remote.cargo_command(triple, pathlib.Path("/cache"), pathlib.Path("/home/me/.cargo"))
            text = " ".join(command)
            self.assertIn(f'target.{triple}.linker="rust-lld"', command)
            flags = json.loads(command[command.index("--config", command.index("--config") + 1) + 1].split("=", 1)[1])
            self.assertEqual(flags, ["-C", "link-arg=--build-id=sha1", "--remap-path-prefix=/home/me/.cargo=/nearshell/cargo"])
            self.assertIn("--features nearshell -p herdr-core --bin nhc --target " + triple, text)
            self.assertNotIn("managed", text)

    def test_only_a_matching_linux_host_runs_the_product(self):
        for system, machine, triple, runs in [("Linux", "x86_64", "x86_64-unknown-linux-musl", True),
                                              ("Linux", "aarch64", "aarch64-unknown-linux-musl", True),
                                              ("Linux", "x86_64", "aarch64-unknown-linux-musl", False),
                                              ("Darwin", "arm64", "aarch64-unknown-linux-musl", False)]:
            with self.subTest(system=system, triple=triple), \
                    mock.patch.object(remote.platform, "system", return_value=system), \
                    mock.patch.object(remote.platform, "machine", return_value=machine):
                self.assertEqual(remote.runs_here(triple), runs)

    def test_manifest_entry_describes_the_file_exactly(self):
        with tempfile.TemporaryDirectory() as temp:
            binary = pathlib.Path(temp) / "nhc-x86_64-unknown-linux-musl"
            binary.write_bytes(b"\x7fELF payload")
            build = remote.expected_build("b" * 40, "p", 2, "release", "com.example")
            self.assertEqual(remote.entry(binary, "x86_64-unknown-linux-musl", build, False),
                             {"target": "x86_64-unknown-linux-musl", "file": binary.name, "bytes": 12,
                              "sha256": hashlib.sha256(b"\x7fELF payload").hexdigest(), "build": build,
                              "executed": False})

    def test_the_manifest_lists_every_target_once_as_a_hub_reads_it(self):
        x86, arm = ({"target": t, "file": f"nhc-{t}"} for t in ("x86_64-unknown-linux-musl",
                                                                "aarch64-unknown-linux-musl"))
        doc = remote.manifest([x86, arm], {"source": {"commit": "c"}})
        self.assertEqual(doc["schemaVersion"], 2)
        self.assertEqual([e["target"] for e in doc["entries"]], sorted(remote.TARGETS))
        self.assertEqual(doc["source"], {"commit": "c"})
        self.assertNotIn("entry", doc)
        with self.assertRaises(ValueError):
            remote.manifest([x86, x86], {})


if __name__ == "__main__":
    unittest.main()
