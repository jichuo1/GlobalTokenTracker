#!/usr/bin/env python3
"""Tests for render_release_template.py (standard library only)."""

from __future__ import annotations

import unittest

from render_release_template import render_release


TEMPLATE = """# Release {{VERSION}}
date {{RELEASE_DATE}} commit {{COMMIT_SHA}}
file {{ASSET_FILENAME}} sha {{ASSET_SHA256}}
{{CHANGELOG}}
"""


class RenderReleaseTemplateTests(unittest.TestCase):
    def test_replaces_all_tokens(self) -> None:
        rendered = render_release(
            TEMPLATE,
            version="v0.2.0",
            commit="abcdef0",
            release_date="2026-01-02",
            sha256="ABC" * 21 + "A",
            asset_filename="GlobalTokenTracker-Setup-v0.2.0-win-x64.exe",
            changelog="- 修复了扫描内存上界",
        )
        self.assertIn("v0.2.0", rendered)
        self.assertIn("修复了扫描内存上界", rendered)
        self.assertNotIn("{{", rendered)

    def test_sha256_is_lowercased(self) -> None:
        rendered = render_release(
            "sha {{ASSET_SHA256}}",
            version="v0.2.0",
            commit="abcdef0",
            release_date="2026-01-02",
            sha256="AA" * 32,
            asset_filename="a.exe",
        )
        self.assertIn("aa" * 32, rendered)

    def test_missing_changelog_raises(self) -> None:
        with self.assertRaises(ValueError):
            render_release(
                "{{CHANGELOG}}",
                version="v0.2.0",
                commit="abcdef0",
                release_date="2026-01-02",
                sha256="aa" * 32,
                asset_filename="a.exe",
                changelog="   ",
            )

    def test_missing_release_summary_raises(self) -> None:
        with self.assertRaises(ValueError):
            render_release(
                "{{RELEASE_SUMMARY}}",
                version="v0.2.0",
                commit="abcdef0",
                release_date="2026-01-02",
                sha256="aa" * 32,
                asset_filename="a.exe",
                release_summary=None,
            )

    def test_unresolved_token_raises(self) -> None:
        with self.assertRaises(ValueError):
            render_release(
                "{{UNKNOWN_TOKEN}}",
                version="v0.2.0",
                commit="abcdef0",
                release_date="2026-01-02",
                sha256="aa" * 32,
                asset_filename="a.exe",
            )


    def test_mac_asset_row_renders_lowercased(self) -> None:
        rendered = render_release(
            "| {{ASSET_FILENAME}} |\n| {{MAC_ASSET_FILENAME}} | {{MAC_ASSET_SHA256}} |",
            version="v0.7.0",
            commit="abcdef0",
            release_date="2026-01-02",
            sha256="aa" * 32,
            asset_filename="GlobalTokenTracker-Setup-v0.7.0-win-x64.exe",
            mac_asset_filename="GlobalTokenTracker-v0.7.0-macos-universal.dmg",
            mac_sha256="BB" * 32,
        )
        self.assertIn("GlobalTokenTracker-v0.7.0-macos-universal.dmg", rendered)
        self.assertIn("bb" * 32, rendered)
        self.assertNotIn("{{", rendered)

    def test_missing_mac_asset_raises(self) -> None:
        with self.assertRaises(ValueError):
            render_release(
                "{{MAC_ASSET_FILENAME}} {{MAC_ASSET_SHA256}}",
                version="v0.7.0",
                commit="abcdef0",
                release_date="2026-01-02",
                sha256="aa" * 32,
                asset_filename="a.exe",
                mac_asset_filename="a.dmg",
                mac_sha256="  ",
            )


if __name__ == "__main__":
    unittest.main()
