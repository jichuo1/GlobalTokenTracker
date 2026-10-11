#!/usr/bin/env python3
"""Render the shared GitHub Release Markdown templates."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


def render_release(
    content: str,
    *,
    version: str,
    commit: str,
    release_date: str,
    sha256: str,
    asset_filename: str,
    release_summary: str | None = None,
    changelog: str | None = None,
    mac_asset_filename: str | None = None,
    mac_sha256: str | None = None,
) -> str:
    replacements = {
        "{{VERSION}}": version,
        "{{COMMIT_SHA}}": commit,
        "{{RELEASE_DATE}}": release_date,
        "{{ASSET_SHA256}}": sha256.lower(),
        "{{ASSET_FILENAME}}": asset_filename,
    }
    # macOS disk image row: required as soon as the template mentions it.
    if "{{MAC_ASSET_FILENAME}}" in content or "{{MAC_ASSET_SHA256}}" in content:
        if not (mac_asset_filename or "").strip() or not (mac_sha256 or "").strip():
            raise ValueError("Template requires the macOS asset filename and SHA-256")
        replacements["{{MAC_ASSET_FILENAME}}"] = mac_asset_filename.strip()
        replacements["{{MAC_ASSET_SHA256}}"] = mac_sha256.strip().lower()
    if "{{RELEASE_SUMMARY}}" in content:
        normalized_summary = (release_summary or "").strip()
        if not normalized_summary:
            raise ValueError("Template requires a non-empty release summary")
        replacements["{{RELEASE_SUMMARY}}"] = normalized_summary
    if "{{CHANGELOG}}" in content:
        normalized_changelog = (changelog or "").strip()
        if not normalized_changelog:
            raise ValueError("Template requires a non-empty changelog")
        replacements["{{CHANGELOG}}"] = normalized_changelog
    for token, value in replacements.items():
        content = content.replace(token, value)

    unresolved = sorted(set(re.findall(r"\{\{[A-Z0-9_]+\}\}", content)))
    if unresolved:
        raise ValueError(f"Unresolved release template tokens: {', '.join(unresolved)}")
    return content


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--template", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--release-date", required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument(
        "--asset-filename",
        help="Published asset filename; defaults to GlobalTokenTracker-Setup-<version>-win-x64.exe",
    )
    parser.add_argument(
        "--mac-asset-filename",
        help="macOS disk image filename, for templates with {{MAC_ASSET_FILENAME}}",
    )
    parser.add_argument(
        "--mac-sha256",
        help="macOS disk image SHA-256, for templates with {{MAC_ASSET_SHA256}}",
    )
    parser.add_argument(
        "--changelog-file",
        type=Path,
        help="Markdown changelog inserted when the template contains {{CHANGELOG}}",
    )
    parser.add_argument(
        "--release-summary",
        help="Stable release overview inserted when the template contains {{RELEASE_SUMMARY}}",
    )
    args = parser.parse_args()

    content = args.template.read_text(encoding="utf-8")
    changelog = (
        args.changelog_file.read_text(encoding="utf-8")
        if args.changelog_file is not None
        else None
    )
    try:
        content = render_release(
            content,
            version=args.version,
            commit=args.commit,
            release_date=args.release_date,
            sha256=args.sha256,
            asset_filename=args.asset_filename
            or f"GlobalTokenTracker-Setup-{args.version}-win-x64.exe",
            release_summary=args.release_summary,
            changelog=changelog,
            mac_asset_filename=args.mac_asset_filename,
            mac_sha256=args.mac_sha256,
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(content, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
