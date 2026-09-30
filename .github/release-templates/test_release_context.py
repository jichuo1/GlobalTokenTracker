#!/usr/bin/env python3
"""Tests for release_context.py (standard library only).

主场景：历史重写（filter-branch 等）后旧发布 tag 仍指向不可达提交，
生成器必须把它识别为孤儿基线——净 diff 仍用它做基，提交记录按日期收敛，
而不是把「无基线」当作首次发布去总结整个历史。
"""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from release_context import (
    build_context,
    find_orphaned_tag,
    find_previous_tag,
    tag_commit_timestamp,
)


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    ).stdout


def _commit(repo: Path, name: str, content: str, message: str) -> str:
    (repo / name).write_text(content, encoding="utf-8")
    _git(repo, "add", name)
    _git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", message, "-q")
    return _git(repo, "rev-parse", "HEAD").strip()


class OrphanBaselineTests(unittest.TestCase):
    """git checkout --orphan 重建历史，模拟 filter-branch 后 tag 悬空。"""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self._tmp.name)
        _git(self.repo, "init", "-q")
        _git(self.repo, "-c", "user.name=t", "-c", "user.email=t", "commit",
             "--allow-empty", "-m", "init", "-q")
        # 旧历史：发布基线 tag + 一些提交
        _commit(self.repo, "a.txt", "v1", "feature A")
        self.old_tip = _commit(self.repo, "b.txt", "v1", "feature B")
        _git(self.repo, "tag", "v1.0.0", self.old_tip)
        # 重写历史：orphan 分支上全新提交（与旧历史不可达）
        _git(self.repo, "checkout", "--orphan", "rewritten", "-q")
        _git(self.repo, "rm", "-rf", ".", "-q")
        _commit(self.repo, "a.txt", "v1-same", "feature A")
        _commit(self.repo, "b.txt", "v1-same", "feature B")
        _commit(self.repo, "c.txt", "v2", "feature C (new release delta)")
        self.head = _git(self.repo, "rev-parse", "HEAD").strip()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_merged_lookup_finds_nothing_after_rewrite(self) -> None:
        self.assertIsNone(
            find_previous_tag(self.repo, self.head, "v1.1.0", "stable")
        )

    def test_orphaned_tag_is_found(self) -> None:
        self.assertEqual(
            find_orphaned_tag(self.repo, self.head, "v1.1.0", "stable"),
            "v1.0.0",
        )

    def test_context_uses_orphan_as_diff_base(self) -> None:
        context = build_context(
            self.repo,
            {},
            channel="stable",
            release_tag="v1.1.0",
            commit=self.head,
            fetch_published=False,
        )
        self.assertTrue(context.baseline_orphaned)
        self.assertEqual(context.previous_tag, "v1.0.0")
        # 提交记录按 tag 日期收敛——orphan 历史里的 feature C 必须在，
        # 且整体不得退回「全部历史」语义（diff 基是 tag 树而非空树）。
        subjects = [c.subject for c in context.commits]
        self.assertIn("feature C (new release delta)", subjects)

    def test_normal_case_unaffected(self) -> None:
        # 无重写的正常仓库：tag 可达 → 非孤儿路径。
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _git(repo, "init", "-q")
            _commit(repo, "a.txt", "1", "base")
            _git(repo, "tag", "v1.0.0")
            head = _commit(repo, "b.txt", "2", "delta")
            context = build_context(
                repo,
                {},
                channel="stable",
                release_tag="v1.1.0",
                commit=head,
                fetch_published=False,
            )
            self.assertFalse(context.baseline_orphaned)
            self.assertEqual(context.previous_tag, "v1.0.0")
            self.assertEqual([c.subject for c in context.commits], ["delta"])

    def test_tag_commit_timestamp_format(self) -> None:
        # ISO 8601 带时分秒：裸日期喂给 --since 会被 approxidate 补成
        # 「今天此刻」，把当天早些时候造的提交全部过滤掉（CI 上曾因此
        # 拿到空提交列表）。
        ts = tag_commit_timestamp(self.repo, "v1.0.0")
        self.assertRegex(ts, r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")


if __name__ == "__main__":
    unittest.main()
