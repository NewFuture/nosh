import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import unittest
from unittest import mock
import zipfile

import release


REPO = Path(__file__).resolve().parents[2]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
VERSION = "0.2.0-rc.1"
COMMIT = "a" * 40


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.root = self.directory / "work"
        self.root.mkdir()
        self.assets = self.directory / "assets"
        self.output = self.directory / "outputs"
        self.manifest = self.root / "Cargo.toml"
        self.manifest.write_text(
            '[workspace]\nmembers = ["crate"]\nresolver = "3"\n'
            '[workspace.package]\nversion = "0.1.0" # application\n'
            '[workspace.metadata]\nversion = "keep-this"\n',
            encoding="utf-8",
        )
        (self.root / "LICENSE").write_text("test license\n", encoding="utf-8")
        (self.root / ".gitattributes").write_text("* text=auto eol=lf\n", encoding="utf-8")
        environment = mock.patch.dict(
            os.environ,
            {
                "GITHUB_REF_TYPE": "branch",
                "GITHUB_REF_NAME": "main",
                "GITHUB_REPOSITORY": "owner/repo",
                "GITHUB_OUTPUT": str(self.output),
            },
        )
        environment.start()
        self.addCleanup(environment.stop)

    def packages(self, version=VERSION):
        for target in TARGETS:
            name = "nosh.exe" if target.endswith("-windows-msvc") else "nosh"
            binary = self.root / "target" / target / "release" / name
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_bytes(b"test executable")
            binary.chmod(0o755)
            with mock.patch.object(
                release.subprocess, "check_output", return_value=f"nosh {version}\n"
            ):
                release.package(self.root, self.assets, version, target)

    def test_full_versions_and_prereleases(self):
        for version, prerelease in [
            ("0.2.0", False),
            (VERSION, True),
            ("1.0.0+build-123", False),
            ("1.0.0-rc.1+build-123", True),
            ("0.0.0-0", True),
        ]:
            with self.subTest(version=version):
                self.assertEqual(
                    release.parse_version(version)["prerelease"] is not None, prerelease
                )
        original = self.manifest.read_bytes()
        for invalid in [
            "", "v0.2.0", "1.2", "01.2.3", "1.02.3", "1.2.03",
            "1.2.3-01", "1.2.3-rc..1", "1.2.3+", "1.2.3\n", '1.2.3"; x=1',
        ]:
            with self.subTest(version=invalid):
                with self.assertRaises(ValueError):
                    release.update_manifest(self.manifest, invalid)
                self.assertEqual(self.manifest.read_bytes(), original)

    def test_manifest_update_is_scoped_and_preserves_formatting(self):
        release.update_manifest(self.manifest, VERSION)
        document = tomllib.loads(self.manifest.read_text(encoding="utf-8"))
        self.assertEqual(document["workspace"]["package"]["version"], VERSION)
        self.assertEqual(document["workspace"]["metadata"]["version"], "keep-this")
        self.assertIn(f'version = "{VERSION}" # application', self.manifest.read_text())
        before = self.manifest.read_bytes()
        release.update_manifest(self.manifest, VERSION)
        self.assertEqual(self.manifest.read_bytes(), before)

    def test_repository_manifest_keeps_all_dependency_and_build_settings(self):
        original = (REPO / "Cargo.toml").read_text(encoding="utf-8")
        self.manifest.write_text(original, encoding="utf-8", newline="\n")
        release.update_manifest(self.manifest, VERSION)
        expected = tomllib.loads(original)
        expected["workspace"]["package"]["version"] = VERSION
        self.assertEqual(
            tomllib.loads(self.manifest.read_text(encoding="utf-8")), expected
        )

    def test_release_keeps_hot_paths_optimized_and_panics_recoverable(self):
        manifest = tomllib.loads((REPO / "Cargo.toml").read_text(encoding="utf-8"))
        profile = manifest["profile"]["release"]
        self.assertEqual(profile["panic"], "unwind")
        self.assertEqual(profile["opt-level"], 3)
        overrides = profile.get("package", {})
        self.assertNotIn("*", overrides)
        for name in [
            "nosh-cli", "nosh-llm", "candle-core", "candle-nn", "gemm",
            "gemm-common", "gemm-f16", "gemm-f32", "gemm-f64",
            "gemm-c32", "gemm-c64", "half", "tokenizers",
            "regex", "regex-automata", "regex-syntax", "fancy-regex",
            "aho-corasick", "memchr", "ring", "rustls",
        ]:
            with self.subTest(package=name):
                self.assertEqual(overrides.get(name, {}).get("opt-level", 3), 3)
        for name in ["brush-core", "brush-parser", "brush-builtins", "libsqlite3-sys"]:
            with self.subTest(package=name):
                self.assertEqual(overrides[name]["opt-level"], "s")

    def test_commits_manifest_and_lockfile_and_pushes_version(self):
        crate = self.root / "crate"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            '[package]\nname = "nosh-release-fixture"\nversion.workspace = true\n'
            'edition = "2024"\n',
            encoding="utf-8",
        )
        (crate / "src" / "lib.rs").write_text("", encoding="utf-8")
        shutil.copyfile(REPO / "rust-toolchain.toml", self.root / "rust-toolchain.toml")
        remote = self.directory / "remote.git"

        def run(*args):
            return subprocess.check_output(args, cwd=self.root, text=True)

        run("cargo", "generate-lockfile", "--offline")
        run("git", "init", "--initial-branch=main")
        run("git", "init", "--bare", "--initial-branch=main", str(remote))
        run("git", "remote", "add", "origin", str(remote))
        run("git", "add", ".")
        run(
            "git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
            "commit", "-m", "initial",
        )
        run("git", "push", "-u", "origin", "main")
        with mock.patch.object(release, "ensure_tag") as tag:
            release.commit_version(self.root, VERSION)
        commit = run("git", "rev-parse", "HEAD").strip()
        tag.assert_called_once_with(VERSION, commit)
        self.assertEqual(run("git", "ls-remote", "origin", "refs/heads/main").split()[0], commit)
        self.assertEqual(
            tomllib.loads((self.root / "Cargo.lock").read_text())["package"][0]["version"],
            VERSION,
        )
        self.assertEqual(
            self.output.read_text().splitlines(), [f"version={VERSION}", f"commit={commit}"]
        )
        self.assertCountEqual(
            run("git", "show", "--format=", "--name-only", "HEAD").splitlines(),
            ["Cargo.toml", "Cargo.lock"],
        )
        self.output.unlink()
        with mock.patch.object(release, "ensure_tag"):
            release.commit_version(self.root, VERSION)
        self.assertEqual(run("git", "rev-parse", "HEAD").strip(), commit)
        self.assertEqual(run("git", "status", "--porcelain"), "")

    def test_tag_dispatch_is_rejected_before_modifying_sources(self):
        original = self.manifest.read_bytes()
        with mock.patch.dict(os.environ, {"GITHUB_REF_TYPE": "tag"}):
            with self.assertRaisesRegex(ValueError, "branch"):
                release.commit_version(self.root, VERSION)
        self.assertEqual(self.manifest.read_bytes(), original)

    def test_push_failure_stops_before_exporting_a_release_commit(self):
        def run(command, **kwargs):
            if command[:2] == ["git", "push"]:
                raise subprocess.CalledProcessError(1, command)
            return subprocess.CompletedProcess(
                command, 1 if command[:2] == ["git", "diff"] else 0
            )

        with (
            mock.patch.object(release.subprocess, "run", side_effect=run),
            mock.patch.object(release.subprocess, "check_output", return_value=COMMIT),
            mock.patch.object(release, "ensure_tag"),
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                release.commit_version(self.root, VERSION)
        self.assertFalse(self.output.exists())

    def test_existing_tag_conflict_prevents_pushing_the_version_commit(self):
        def run(command, **kwargs):
            return subprocess.CompletedProcess(
                command, 1 if command[:2] == ["git", "diff"] else 0
            )

        with (
            mock.patch.object(release.subprocess, "run", side_effect=run) as commands,
            mock.patch.object(release.subprocess, "check_output", return_value=COMMIT),
            mock.patch.object(release, "ensure_tag", side_effect=ValueError("Tag conflict")),
        ):
            with self.assertRaisesRegex(ValueError, "Tag conflict"):
                release.commit_version(self.root, VERSION)
        self.assertFalse(
            any(call.args[0][:2] == ["git", "push"] for call in commands.call_args_list)
        )
        self.assertFalse(self.output.exists())

    def test_archives_include_native_executable_and_license(self):
        self.packages()
        self.assertEqual(len(list(self.assets.glob("*.sha256"))), len(TARGETS))
        for target in TARGETS:
            if target.endswith("-windows-msvc"):
                path = self.assets / f"nosh-{VERSION}-{target}.zip"
                with zipfile.ZipFile(path) as archive:
                    self.assertCountEqual(archive.namelist(), ["nosh.exe", "LICENSE"])
                    self.assertEqual(archive.read("nosh.exe"), b"test executable")
            else:
                path = self.assets / f"nosh-{VERSION}-{target}.tar.gz"
                with tarfile.open(path) as archive:
                    self.assertCountEqual(archive.getnames(), ["nosh", "LICENSE"])
                    self.assertTrue(archive.getmember("nosh").mode & 0o111)
            self.assertEqual(
                path.with_name(path.name + ".sha256").read_text(), release.checksum(path)
            )

    def test_windows_archive_uses_maximum_deflate_compression(self):
        with mock.patch.object(
            release.zipfile, "ZipFile", wraps=zipfile.ZipFile
        ) as archive:
            self.packages()
        archive.assert_called_once_with(
            self.assets / f"nosh-{VERSION}-{TARGETS[-1]}.zip",
            "w",
            compression=zipfile.ZIP_DEFLATED,
            compresslevel=9,
        )

    def test_wrong_executable_version_cannot_be_packaged(self):
        with mock.patch.object(
            release.subprocess, "check_output", return_value="nosh 0.1.0\n"
        ):
            with self.assertRaisesRegex(ValueError, "executable version"):
                release.package(self.root, self.assets, VERSION, TARGETS[0])
        self.assertFalse(self.assets.exists())

    def test_every_platform_and_checksum_is_required_before_publication(self):
        self.packages()
        with mock.patch.object(release.subprocess, "check_output") as api:
            archive = next(self.assets.glob("*.zip"))
            archive.unlink()
            with self.assertRaisesRegex(ValueError, "found 4"):
                release.publish(self.assets, VERSION, COMMIT, len(TARGETS))
            archive.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
                release.publish(self.assets, VERSION, COMMIT, len(TARGETS))
            api.assert_not_called()

    def test_publishes_all_archives_together_for_the_version_commit(self):
        self.packages()
        with mock.patch.object(release.subprocess, "check_output", return_value="[]"):
            with mock.patch.object(release.subprocess, "run") as run:
                release.publish(self.assets, VERSION, COMMIT, len(TARGETS))
        command = run.call_args.args[0]
        self.assertEqual(command[:4], ["gh", "release", "create", f"v{VERSION}"])
        files = command[4 : 4 + len(TARGETS) + 1]
        self.assertEqual(len(files), len(TARGETS) + 1)
        self.assertEqual(Path(files[-1]).name, "SHA256SUMS")
        self.assertEqual(command[command.index("--target") + 1], COMMIT)
        self.assertIn("--prerelease=true", command)
        self.assertNotIn("--clobber", command)
        self.assertEqual(
            len((self.assets / "SHA256SUMS").read_text().splitlines()), len(TARGETS)
        )
        self.assertTrue(run.call_args.kwargs["check"])

    def test_build_metadata_does_not_mark_a_stable_release_as_prerelease(self):
        version = "0.2.0+build-123"
        self.packages(version)
        with mock.patch.object(release.subprocess, "check_output", return_value="[]"):
            with mock.patch.object(release.subprocess, "run") as run:
                release.publish(self.assets, version, COMMIT, len(TARGETS))
        self.assertIn("--prerelease=false", run.call_args.args[0])

    def test_conflicting_tag_and_failed_publication_are_not_hidden(self):
        self.packages()
        refs = [
            {"ref": f"refs/tags/v{VERSION}", "object": {"type": "commit", "sha": "b" * 40}}
        ]
        with mock.patch.object(
            release.subprocess, "check_output", return_value=json.dumps(refs)
        ):
            with mock.patch.object(release.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "does not point"):
                    release.publish(self.assets, VERSION, COMMIT, len(TARGETS))
                run.assert_not_called()
        with mock.patch.object(release.subprocess, "check_output", return_value="[]"):
            with mock.patch.object(
                release.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "gh")
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    release.publish(self.assets, VERSION, COMMIT, len(TARGETS))
        with mock.patch.object(
            release.subprocess, "check_output",
            side_effect=subprocess.CalledProcessError(1, "gh"),
        ):
            with mock.patch.object(release.subprocess, "run") as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    release.publish(self.assets, VERSION, COMMIT, len(TARGETS))
                run.assert_not_called()

    def test_workflow_builds_every_supported_target_from_the_version_commit(self):
        workflow = (REPO / ".github" / "workflows" / "release.yml").read_text(encoding="utf-8")
        self.assertCountEqual(
            re.findall(r"^\s+target: (\S+)$", workflow, re.MULTILINE), TARGETS
        )
        self.assertEqual(workflow.count("ref: ${{ needs.version.outputs.commit }}"), 2)
        self.assertIn("needs: [version, build]", workflow)
        self.assertIn("artifact_count: ${{ strategy.job-total }}", workflow)
        self.assertIn("overwrite: true", workflow)


if __name__ == "__main__":
    unittest.main()
