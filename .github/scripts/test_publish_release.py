import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("publisher", Path(__file__).with_name("publish-release.py"))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


class PublishingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.tag = "v0.1.0"
        self.revision = "a" * 40
        self.archive = "bkmqb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        (self.directory / self.archive).write_bytes(b"verified archive")
        (self.directory / "SHA256SUMS").write_text(
            publisher.digest(self.directory / self.archive) + "  " + self.archive + "\n")
        self.assets = {}
        self.exists = False
        self.draft = True
        self.moves = False
        self.annotated = False
        self.fail_upload = False
        self.commands = []

    def run_gh(self, args, **kwargs):
        self.assertEqual(args[0], "gh")
        self.assertNotIn("--clobber", args)
        self.commands.append(args)
        result = None
        if args[1] == "api":
            path = args[2]
            if path == "graphql":
                result = {"data": {"repository": {"release": {"databaseId": 7} if self.exists else None}}}
            elif "/git/ref/" in path:
                result = {"object": {"type": "tag" if self.annotated else "commit",
                                     "sha": "b" * 40 if self.moves else self.revision}}
            elif "/git/tags/" in path:
                result = {"object": {"type": "commit", "sha": self.revision}}
            elif "/releases/assets/" in path:
                name = sorted(self.assets)[int(path.rsplit("/", 1)[1])]
                kwargs["stdout"].write(self.assets[name])
            elif "/releases/tags/" in path:
                self.fail("draft lookup must not use the published-tag REST endpoint")
            elif self.exists:
                result = {"draft": self.draft, "html_url": "https://github.com/example/repo/releases/tag/v0.1.0",
                          "assets": [{"name": name, "id": index}
                                     for index, name in enumerate(sorted(self.assets))]}
            else:
                return subprocess.CompletedProcess(args, 1, "", "HTTP 404")
        elif args[2] == "create":
            self.assertIn("--draft", args)
            self.exists = True
        elif args[2] == "upload":
            if self.fail_upload:
                self.fail_upload = False
                raise subprocess.CalledProcessError(1, args)
            path = Path(args[4])
            self.assertNotIn(path.name, self.assets)
            self.assets[path.name] = path.read_bytes()
        elif args[2] == "edit":
            self.assertIn("--draft=false", args)
            self.assertEqual(set(self.assets), {self.archive, "SHA256SUMS"})
            self.draft = False
        else:
            self.fail("unexpected gh command")
        return subprocess.CompletedProcess(args, 0, json.dumps(result), "")

    def publish(self):
        with patch.object(publisher.subprocess, "run", side_effect=self.run_gh):
            publisher.publish(self.directory, Path("notes.md"), "example/repo", self.tag, self.revision)

    def test_new_release_and_identical_rerun(self):
        self.publish()
        self.assertFalse(self.draft)
        self.commands.clear()
        self.publish()
        self.assertFalse(any(c[1] == "release" for c in self.commands))

    def test_interrupted_upload_remains_draft_and_resumes(self):
        self.fail_upload = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.publish()
        self.assertTrue(self.exists and self.draft)
        self.publish()
        self.assertFalse(self.draft)

    def test_draft_only_uploads_missing_asset(self):
        self.exists = True
        self.assets[self.archive] = (self.directory / self.archive).read_bytes()
        self.publish()
        self.assertEqual(sum(c[1:3] == ["release", "upload"] for c in self.commands), 1)

    def test_conflicting_asset_is_not_overwritten_or_published(self):
        self.exists = True
        self.assets = {self.archive: b"different", "SHA256SUMS": (self.directory / "SHA256SUMS").read_bytes()}
        with self.assertRaisesRegex(RuntimeError, "differs"):
            self.publish()
        self.assertTrue(self.draft)
        self.assertEqual(self.assets[self.archive], b"different")

    def test_moved_tag_fails_before_creation(self):
        self.moves = True
        with self.assertRaisesRegex(RuntimeError, "tag no longer matches"):
            self.publish()
        self.assertFalse(self.exists)

    def test_annotated_tag_is_peeled(self):
        self.annotated = True
        self.publish()
        self.assertFalse(self.draft)

    def test_incomplete_published_release_is_not_changed(self):
        self.exists, self.draft = True, False
        with self.assertRaisesRegex(RuntimeError, "published release is incomplete"):
            self.publish()
        self.assertFalse(any(c[1] == "release" for c in self.commands))


if __name__ == "__main__":
    unittest.main()
