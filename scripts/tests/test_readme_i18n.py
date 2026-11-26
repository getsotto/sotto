"""README i18n checker behaviour; the shipped translations are fixtures."""

import importlib.machinery
import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "readme_i18n", str(ROOT / "scripts/check-readme-i18n")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
checker = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(checker)

SOURCE = """# Title

Text with `sotto init` and a [link](docs/CLI.md) plus https://example.com/x.

```sh
sotto init   # comment
```

## Section

| A | B |
| --- | --- |
| c | d |

> [!WARNING]
> Careful.

[en](README.md) | [es](README.es.md)
"""

TRANSLATION = SOURCE.replace("# Title", "# Título").replace(
    "Text with", "Texto con"
)


class HelperTest(unittest.TestCase):
    def test_identical_translation_passes(self):
        self.assertEqual(checker.check_translation(SOURCE, TRANSLATION, "T"), [])

    def test_changed_code_block_fails(self):
        bad = TRANSLATION.replace("sotto init   # comment", "sotto init   # comentario")
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("code blocks" in e for e in errors))

    def test_translated_command_fails(self):
        bad = TRANSLATION.replace("`sotto init`", "`sotto inicio`", 1)
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("inline code" in e for e in errors))

    def test_extra_code_span_fails(self):
        bad = TRANSLATION.replace("Texto con", "Texto `nuevo` con")
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("inline code" in e for e in errors))

    def test_changed_url_fails(self):
        bad = TRANSLATION.replace("https://example.com/x", "https://example.com/y")
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("URLs" in e for e in errors))

    def test_changed_destination_fails(self):
        bad = TRANSLATION.replace("docs/CLI.md", "docs/AUTRE.md")
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("destinations" in e for e in errors))

    def test_fragment_and_nav_links_ignored(self):
        text = SOURCE + "\n[en](README.md) | [es](README.es.md) | [Sec](#sección)\n"
        self.assertEqual(checker.link_destinations(text), checker.link_destinations(SOURCE))

    def test_changed_heading_structure_fails(self):
        bad = TRANSLATION.replace("## Section", "### Section")
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("heading" in e for e in errors))

    def test_dropped_table_row_fails(self):
        bad = TRANSLATION.replace("| c | d |\n", "")
        errors = checker.check_translation(SOURCE, bad, "T")
        self.assertTrue(any("table" in e for e in errors))

    def test_missing_backlink_fails(self):
        bad = TRANSLATION.replace("(README.md)", "(readme.md)")
        errors = [e for e in checker.check_translation(SOURCE, bad, "T") if "back" in e]
        self.assertEqual(len(errors), 1)

    def test_unclosed_fence_raises(self):
        with self.assertRaises(ValueError):
            checker.fenced_blocks("```sh\nnever closed\n")


class UnclosedFenceTest(unittest.TestCase):
    """An unmatched fence is a validation message, not a traceback."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        for name in (checker.SOURCE,) + checker.TRANSLATIONS:
            shutil.copy(ROOT / name, self.tmp / name)
        self.assertEqual(checker.check_root(self.tmp), [])

    def append_fence(self, name):
        path = self.tmp / name
        text = path.read_text(encoding="utf-8")
        path.write_text(text + "\n```\n", encoding="utf-8")
        return len(text.splitlines()) + 2

    def test_fence_error_reports_line_number(self):
        with self.assertRaisesRegex(ValueError, "opened on line 2"):
            checker.fenced_blocks("text\n```sh\nnever closed\n")

    def test_unclosed_fence_in_translation_names_file_and_line(self):
        line = self.append_fence("README.fr.md")
        self.assertEqual(
            checker.check_root(self.tmp),
            [f"README.fr.md: unclosed fenced code block opened on line {line}"],
        )

    def test_unclosed_fence_in_source_names_file_and_line(self):
        line = self.append_fence("README.md")
        self.assertEqual(
            checker.check_root(self.tmp),
            [f"README.md: unclosed fenced code block opened on line {line}"],
        )

    def test_command_exits_nonzero_without_traceback(self):
        self.append_fence("README.fr.md")
        out = subprocess.run(
            [sys.executable, str(ROOT / "scripts/check-readme-i18n"), "--root", str(self.tmp)],
            capture_output=True, text=True,
        )
        self.assertEqual(out.returncode, 1)
        self.assertIn("README.fr.md: unclosed fenced code block opened on line", out.stdout)
        self.assertNotIn("Traceback", out.stdout + out.stderr)

    def test_closing_the_fence_passes_again(self):
        self.append_fence("README.de.md")
        self.assertNotEqual(checker.check_root(self.tmp), [])
        shutil.copy(ROOT / "README.de.md", self.tmp / "README.de.md")
        self.assertEqual(checker.check_root(self.tmp), [])


class RepoTest(unittest.TestCase):
    def test_shipped_translations_in_sync(self):
        self.assertEqual(checker.check_root(ROOT), [])

    def test_source_links_every_translation(self):
        source = (ROOT / "README.md").read_text(encoding="utf-8")
        for name in checker.TRANSLATIONS:
            self.assertIn(f"({name})", source)


if __name__ == "__main__":
    unittest.main()
