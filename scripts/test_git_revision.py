#!/usr/bin/env python3
"""Exercise the real core build script without building model dependencies."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "mistralrs-core" / "build.rs"
ENV = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
ENV.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)


def git(root, *args):
    return subprocess.check_output(
        [
            "git", "-c", "user.name=Build Script Test",
            "-c", "user.email=build-script@example.invalid",
            "-c", "commit.gpgsign=false", "-c", "protocol.file.allow=always",
            "-C", str(root), *args,
        ],
        env=ENV, text=True, stderr=subprocess.PIPE,
    ).strip()


class GitRevisionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.compiled = tempfile.TemporaryDirectory(prefix="mistral-build-script-")
        cls.binary = Path(cls.compiled.name) / "build-script"
        subprocess.run(
            ["rustc", "--edition=2021", "-D", "warnings", str(SOURCE), "-o", str(cls.binary)],
            check=True,
        )

    @classmethod
    def tearDownClass(cls):
        cls.compiled.cleanup()

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="mistral-git-fixture-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def repository(self, name="repo"):
        root = self.root / name
        package = root / "core"
        package.mkdir(parents=True)
        shutil.copyfile(SOURCE, package / "build.rs")
        git(root, "init", "--quiet", "--initial-branch=main")
        git(root, "add", "core/build.rs")
        git(root, "commit", "--quiet", "-m", "initial")
        return root, package

    def run_script(self, package):
        output = subprocess.check_output(
            [str(self.binary)], cwd=package, env=ENV, text=True,
        )
        lines = output.splitlines()
        revision = next(
            line.removeprefix("cargo:rustc-env=MISTRALRS_GIT_REVISION=")
            for line in lines if line.startswith("cargo:rustc-env=MISTRALRS_GIT_REVISION=")
        )
        watched = {
            (package / line.removeprefix("cargo:rerun-if-changed=")).resolve()
            for line in lines if line.startswith("cargo:rerun-if-changed=")
        }
        self.assertIn(package / "build.rs", watched)
        for path in watched:
            self.assertTrue(path.exists(), f"missing Cargo input: {path}")
        return revision, watched

    def metadata(self, package, name):
        return Path(git(package, "rev-parse", "--path-format=absolute", "--git-path", name))

    def assert_revision(self, package):
        revision, watched = self.run_script(package)
        self.assertEqual(revision, git(package, "rev-parse", "HEAD"))
        self.assertIn(self.metadata(package, "HEAD"), watched)
        self.assertNotIn(self.metadata(package, "HEAD").parent, watched)
        return watched

    def test_normal_checkout_and_commit(self):
        root, package = self.repository()
        first = self.run_script(package)
        self.assertEqual(first, self.run_script(package))
        self.assertIn(self.metadata(package, "refs/heads/main"), self.assert_revision(package))
        git(root, "commit", "--quiet", "--allow-empty", "-m", "next")
        self.assertNotEqual(first[0], self.run_script(package)[0])
        self.assert_revision(package)

    def test_submodule(self):
        upstream, _ = self.repository("upstream")
        parent, _ = self.repository("parent")
        git(parent, "submodule", "add", "--quiet", str(upstream), "vendor")
        package = parent / "vendor" / "core"
        self.assertTrue((package.parent / ".git").is_file())
        self.assert_revision(package)

    def test_linked_worktree(self):
        root, _ = self.repository()
        linked = self.root / "linked"
        git(root, "worktree", "add", "--quiet", "-b", "linked", str(linked))
        package = linked / "core"
        watched = self.assert_revision(package)
        self.assertIn(self.metadata(package, "refs/heads/linked"), watched)
        self.assertNotEqual(
            self.metadata(package, "HEAD").parent,
            self.metadata(package, "refs").parent,
        )

    def test_detached_head(self):
        root, package = self.repository()
        git(root, "checkout", "--quiet", "--detach")
        watched = self.assert_revision(package)
        self.assertEqual(watched, {package / "build.rs", self.metadata(package, "HEAD")})

    def test_loose_packed_loose_transitions(self):
        root, package = self.repository()
        git(root, "branch", "-m", "nested/topic")
        reference = self.metadata(package, "refs/heads/nested/topic")
        initial = self.assert_revision(package)
        self.assertIn(reference, initial)
        git(root, "pack-refs", "--all", "--prune")
        self.assertFalse(reference.exists())
        if reference.parent.is_dir():
            reference.parent.rmdir()
        packed = self.metadata(package, "packed-refs")
        watched = self.assert_revision(package)
        self.assertIn(packed, watched)
        self.assertIn(self.metadata(package, "refs/heads"), watched)
        packed_contents = packed.read_bytes()
        git(root, "commit", "--quiet", "--allow-empty", "-m", "loose again")
        self.assertTrue(reference.exists())
        self.assertEqual(packed_contents, packed.read_bytes())
        self.assertIn(reference, self.assert_revision(package))

    def test_source_archive_without_git(self):
        package = self.root / "archive"
        package.mkdir()
        shutil.copyfile(SOURCE, package / "build.rs")
        self.assertEqual(self.run_script(package), ("unknown", {package / "build.rs"}))

    def test_untracked_archive_inside_another_repository(self):
        root, _ = self.repository()
        package = root / "archive"
        package.mkdir()
        shutil.copyfile(SOURCE, package / "build.rs")
        self.assertEqual(self.run_script(package), ("unknown", {package / "build.rs"}))


if __name__ == "__main__":
    unittest.main(verbosity=2)
