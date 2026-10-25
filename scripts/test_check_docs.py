import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check_docs  # noqa: E402

GOOD_YAML = "root: ./docs/\n\nstructure:\n  readme: README.md\n  summary: SUMMARY.md\n"
SUMMARY = "# Table of contents\n\n* [Precept](README.md)\n* [A](a.md)\n"


class CheckDocsTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.top = self._tmp.name
        self.root = os.path.join(self.top, "docs")
        self.put(".gitbook.yaml", GOOD_YAML)
        self.put("docs/SUMMARY.md", SUMMARY)
        self.put("docs/README.md", "# Precept\n\nSee [a](a.md) and [site](https://example.com).\n")
        self.put("docs/a.md", "# A\n\n[top](README.md#x) [here](#h)\n")

    def put(self, rel, text):
        path = os.path.join(self.top, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)

    def test_clean(self):
        self.assertEqual(check_docs.check(self.root), [])

    def test_missing_summary_target(self):
        self.put("docs/SUMMARY.md", SUMMARY + "* [B](b.md)\n")
        self.assertTrue(any("b.md does not exist" in p for p in check_docs.check(self.root)))

    def test_duplicate_summary_entry(self):
        self.put("docs/SUMMARY.md", SUMMARY + "* [A again](a.md)\n")
        self.assertTrue(any("a.md listed 2 times" in p for p in check_docs.check(self.root)))

    def test_link_escaping_root(self):
        self.put("outside.md", "x\n")
        self.put("docs/a.md", "[o](../outside.md)\n")
        self.assertTrue(any("escapes" in p for p in check_docs.check(self.root)))

    def test_dangling_link(self):
        self.put("docs/a.md", "[n](nope.md)\n")
        self.assertTrue(any("nope.md does not exist" in p for p in check_docs.check(self.root)))

    def test_unlisted_file_ignored(self):
        self.put("docs/emergency-runbooks/r.md", "[bad](missing.md) [esc](../../x.md)\n")
        self.assertEqual(check_docs.check(self.root), [])

    def test_wrong_gitbook_root(self):
        self.put(".gitbook.yaml", GOOD_YAML.replace("./docs/", "./site/"))
        self.assertTrue(any("root must be ./docs/" in p for p in check_docs.check(self.root)))


if __name__ == "__main__":
    unittest.main()
