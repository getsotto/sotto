"""README i18n checker behaviour; the shipped translations are fixtures."""

import importlib.machinery
import importlib.util
from pathlib import Path
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

    def test_important_downgraded_to_tip_fails(self):
        source = SOURCE.replace("[!WARNING]", "[!IMPORTANT]")
        tip = source.replace("[!IMPORTANT]", "[!TIP]")
        self.assertEqual(checker.check_translation(source, source, "T"), [])
        errors = checker.check_translation(source, tip, "T")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("[!IMPORTANT] 1 in README.md vs 0 in T", errors[0])
        restored = tip.replace("[!TIP]", "[!IMPORTANT]")
        self.assertEqual(checker.check_translation(source, restored, "T"), [])

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


class RepoTest(unittest.TestCase):
    def test_shipped_translations_in_sync(self):
        self.assertEqual(checker.check_root(ROOT), [])

    def test_source_links_every_translation(self):
        source = (ROOT / "README.md").read_text(encoding="utf-8")
        for name in checker.TRANSLATIONS:
            self.assertIn(f"({name})", source)


if __name__ == "__main__":
    unittest.main()
