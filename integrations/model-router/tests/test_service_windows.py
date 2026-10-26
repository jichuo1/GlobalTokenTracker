import tempfile
import time
import unittest
from pathlib import Path

from gtt_router.api import dispatch
from tests.supervised_service import SupervisedService as RouterService
from tests.test_router import FakeCatalog, route, response


class ServiceWindowsTest(unittest.TestCase):
    def service(self, adapter):
        folder = tempfile.TemporaryDirectory()
        service = RouterService({'state_dir': folder.name, 'limits': {'max_active_jobs': 1},
                                 'recovery': {'enabled': False}},
                                catalog=FakeCatalog([route()]), adapters={'fake': adapter})
        self.addCleanup(folder.cleanup)
        self.addCleanup(service.shutdown)
        return service

    def wait(self, service, job_id):
        end = time.monotonic() + 5
        while time.monotonic() < end:
            status = service.status(job_id)
            if status['state'] in {'succeeded', 'failed', 'partial', 'needs_codex'}:
                return status
            time.sleep(.02)
        self.fail('Fixture job did not complete')

    def test_hard_mode_does_not_use_short_ttft_as_absolute_limit(self):
        observed = []
        def adapter(r, prompt, task, ctx):
            observed.append(ctx.deadline - time.monotonic())
            return response()
        service = self.service(adapter)
        job = service.submit({'kind': 'extract', 'prompt': 'x', 'timeout_mode': 'hard',
                              'deadline_s': 600, 'ttft_s': 1, 'allow_fallback': False,
                              'max_attempts': 1})
        self.assertEqual(self.wait(service, job['job_id'])['state'], 'succeeded')
        self.assertGreater(observed[0], 590)

    def test_verifier_keeps_initial_window_and_price_preferences(self):
        seen = []
        def adapter(r, prompt, task, ctx):
            seen.append((task.kind, task.deadline_s, task.price_weight, task.prefer_free, ctx.public_activity()))
            return response()
        service = self.service(adapter)
        job = service.submit({'kind': 'extract', 'prompt': 'x', 'deadline_s': 180,
                              'verify': True, 'max_attempts': 2, 'price_weight': 55, 'prefer_free': False})
        self.assertEqual(self.wait(service, job['job_id'])['state'], 'succeeded')
        self.assertEqual(seen[1][0], 'cross_check')
        self.assertLessEqual(seen[1][1], 180)
        self.assertEqual(seen[1][2:4], (55, False))

    def test_waiting_baseline_is_visible_without_claimed_activity(self):
        def adapter(r, prompt, task, ctx):
            ctx.cancel.wait(.15)
            return response()
        service = self.service(adapter)
        job = service.submit({'kind': 'extract', 'prompt': 'x', 'deadline_s': 180,
                              'workflow_id': 'waiting-test', 'max_attempts': 1})
        end = time.monotonic() + 2
        status = None
        while time.monotonic() < end:
            data = dispatch(service, 'router_supervisor', {'action': 'status', 'workflow_id': 'waiting-test'})
            if data['jobs'] and data['jobs'][0].get('progress'):
                status = data['jobs'][0]
                break
            time.sleep(.01)
        self.assertIsNotNone(status)
        activity = next(iter(status['progress'].values()))['activity']
        self.assertEqual(activity['activity_count'], 0)
        self.assertGreater(activity['hard_remaining_s'], 1000)

    def test_activity_plan_uses_total_horizon_but_final_only_honors_first_window(self):
        r = route(latency=20)
        service = self.service(lambda *args: response())
        service.catalog.routes = [r]
        task = {'kind': 'extract', 'prompt': 'x', 'deadline_s': 10, 'hard_deadline_s': 120}
        self.assertEqual(len(service.plan(task)['candidates']), 1)
        r.catalog_metadata['activity_observability'] = 'final_only'
        self.assertFalse(service.plan(task)['candidates'])
        task['ttft_s'] = 30
        self.assertEqual(len(service.plan(task)['candidates']), 1)


if __name__ == '__main__':
    unittest.main()
