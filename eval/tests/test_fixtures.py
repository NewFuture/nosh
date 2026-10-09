from __future__ import annotations
from pathlib import Path
import socket
import sys
import tempfile
import unittest
from eval import fixtures


@unittest.skipUnless(sys.platform == "linux", "Linux fixtures and process interfaces")
class FixtureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_fixture_rebuild_and_exact_facts(self):
        with fixtures.Workspace(self.base / "work") as workspace:
            scenario = {"id": "project", "fixture": "project"}
            root, _, first = workspace.prepare(scenario)
            self.assertNotIn(scenario["id"], root.parts)
            self.assertEqual(root.parent, workspace.case_path(scenario["id"]))
            (root / "main.py").write_text("changed")
            root, _, second = workspace.prepare(scenario)
            self.assertEqual(first, second)
            self.assertEqual(second["languages"], {"python": 15, "javascript": 4, "rust": 6, "shell": 4})
            self.assertEqual(second["total"], 29)
            self.assertEqual(len(second["python"]), 3)
            self.assertTrue(all(int(p.stat().st_mtime) == fixtures.EPOCH for p in root.rglob("*")))

    def test_fixed_git_history(self):
        a, b = self.base / "a", self.base / "b"
        first = fixtures.create(a, "history")
        second = fixtures.create(b, "history")
        self.assertEqual(first, second)
        self.assertEqual(fixtures.git(a, "rev-list", "--count", "HEAD").strip(), "8")

    def test_directory_snapshots_include_empty_directories_without_following_links(self):
        root = self.base / "files"
        root.mkdir()
        (root / "empty").mkdir()
        (root / "nested" / "empty").mkdir(parents=True)
        (root / ".git" / "objects").mkdir(parents=True)
        outside = self.base / "outside"
        (outside / "private").mkdir(parents=True)
        (root / "linked").symlink_to(outside, target_is_directory=True)
        self.assertEqual(fixtures.directory_snapshot(root), [".git", "empty", "nested", "nested/empty"])
        self.assertEqual(fixtures.snapshot(root), {"linked": {"symlink": str(outside)}})
        links = self.base / "links"
        links.mkdir()
        (links / ".git").symlink_to(outside, target_is_directory=True)
        self.assertEqual(fixtures.directory_snapshot(links), [])
        self.assertEqual(fixtures.snapshot(links), {".git": {"symlink": str(outside)}})

    def test_large_files_are_materialized_not_sparse(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        allocated = {name: (root / name).stat().st_blocks * 512 for name in facts["before"]}
        self.assertTrue(all(size > 0 for size in allocated.values()))
        self.assertEqual(sorted(allocated, key=allocated.get, reverse=True)[:3], facts["largest"])
        self.assertEqual((root / "data" / "dump.bin").stat().st_size, 21_000_000)

    def test_ownership_lock_and_symlink_boundaries(self):
        outsider = self.base / "outside"
        outsider.mkdir()
        (outsider / "keep").write_text("keep")
        with self.assertRaises(ValueError):
            with fixtures.Workspace(outsider):
                pass
        public = self.base / "public"
        public.mkdir(mode=0o755)
        with self.assertRaisesRegex(ValueError, "private"):
            with fixtures.Workspace(public):
                pass
        with fixtures.Workspace(self.base / "work") as workspace:
            with self.assertRaises(RuntimeError):
                with fixtures.Workspace(workspace.root):
                    pass
            with self.assertRaises(ValueError):
                workspace.clean("../outside")
            workspace.case_path("linked").symlink_to(outsider, target_is_directory=True)
            workspace.clean("linked")
            self.assertFalse(workspace.case_path("linked").exists())
            self.assertEqual((outsider / "keep").read_text(), "keep")
        with self.assertRaises(ValueError):
            workspace.clean("linked")
        (self.base / "link").symlink_to(outsider, target_is_directory=True)
        with self.assertRaises(ValueError):
            with fixtures.Workspace(self.base / "link" / "child"):
                pass

    def test_listener_owns_pid_and_never_replaces_occupied_port(self):
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            port = occupied.getsockname()[1]
            with self.assertRaises(RuntimeError):
                with fixtures.listener(self.base, port):
                    pass
        with fixtures.listener(self.base, port) as info:
            self.assertEqual(info["port"], port)
            self.assertTrue(Path(f"/proc/{info['pid']}").exists())
        self.assertFalse(Path(f"/proc/{info['pid']}").exists())
