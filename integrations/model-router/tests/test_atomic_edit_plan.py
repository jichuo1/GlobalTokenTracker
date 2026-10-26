import copy
import unittest

import test_operator as fixtures


class AtomicEditPlanTests(unittest.TestCase):
    setUp = fixtures.OperatorTests.setUp
    tearDown = fixtures.OperatorTests.tearDown
    execute = fixtures.OperatorTests.execute
    command = fixtures.OperatorTests.command

    def edits(self, body="alpha=1\r\nbeta=2\r\n"):
        (self.workspace / "a.txt").write_bytes(body.encode())
        snapshot = self.broker.snapshot(self.grant, ["a.txt"])[0]
        operations = [{"id": ident, "op": "replace_text", "path": "a.txt", "base_sha256": snapshot["sha256"],
            "old_text": old, "new_text": new} for ident, old, new in [("first", "alpha=1", "alpha=10"), ("second", "beta=2", "beta=20")]]
        return snapshot, operations

    def test_two_independent_replacements_become_one_idempotent_atomic_receipt(self):
        snapshot, edits = self.edits()
        original = copy.deepcopy(edits)
        operations, bundles = self.broker.plan_operations(self.grant, ["a.txt"], edits, [snapshot])
        self.assertEqual(len(operations), 1)
        self.assertEqual(bundles[0]["source_operation_ids"], ["first", "second"])
        result = self.execute(operations)
        self.assertEqual(result["state"], "applied")
        self.assertEqual(len(result["receipts"]), 1)
        self.assertEqual((self.workspace / "a.txt").read_bytes(), b"alpha=10\r\nbeta=20\r\n")
        repeated, again = self.broker.plan_operations(self.grant, ["a.txt"], edits, [snapshot])
        self.assertEqual((repeated, again), (operations, bundles))
        self.assertTrue(self.execute(repeated)["receipts"][0]["deduplicated"])
        self.assertEqual(edits, original)

    def test_overlap_duplicate_ids_and_mismatched_baselines_remain_rejected(self):
        snapshot, edits = self.edits()
        variants = []
        overlap = copy.deepcopy(edits)
        overlap[1]["old_text"] = "pha=1"
        variants.append(overlap)
        duplicate = copy.deepcopy(edits)
        duplicate[1]["id"] = duplicate[0]["id"]
        variants.append(duplicate)
        stale = copy.deepcopy(edits)
        stale[1]["base_sha256"] = "f" * 64
        variants.append(stale)
        unknown = copy.deepcopy(edits)
        unknown[1]["untrusted_field"] = True
        variants.append(unknown)
        for value in variants:
            with self.assertRaises(ValueError):
                self.broker.plan_operations(self.grant, ["a.txt"], value, [snapshot])
        self.assertEqual((self.workspace / "a.txt").read_bytes(), snapshot["content"].encode())

    def test_slice_bundle_stays_inside_authorized_excerpt_and_preserves_surroundings(self):
        snapshot, edits = self.edits("HEAD\r\nalpha=1\r\nbeta=2\r\nTAIL\r\n")
        snapshot.update(content="alpha=1\r\nbeta=2\r\n", excerpt_only=True, line_range=[2, 3])
        operations, bundles = self.broker.plan_operations(self.grant, ["a.txt"], edits, [snapshot])
        self.assertTrue(bundles[0]["snapshot_excerpt_only"])
        self.assertEqual(self.execute(operations)["state"], "applied")
        self.assertEqual((self.workspace / "a.txt").read_bytes(), b"HEAD\r\nalpha=10\r\nbeta=20\r\nTAIL\r\n")
        edits[0]["old_text"] = "HEAD"
        with self.assertRaisesRegex(ValueError, "unique matches"):
            self.broker.plan_operations(self.grant, ["a.txt"], edits, [snapshot])

    def test_command_boundaries_and_byte_limits_cannot_be_hidden_by_bundling(self):
        snapshot, edits = self.edits()
        command = self.command("pass")
        with self.assertRaisesRegex(ValueError, "separate reviewed batches"):
            self.broker.plan_operations(self.grant, ["a.txt"], [edits[0], command, edits[1]], [snapshot])
        self.grant["max_file_bytes"] = 24
        for edit in edits:
            edit["new_text"] = "x" * 20
        with self.assertRaises(ValueError):
            self.broker.plan_operations(self.grant, ["a.txt"], edits, [snapshot])
