import tempfile
import unittest
from pathlib import Path

from gtt_router.models import Route, Task, now_ms
from gtt_router.pricing import normalize_pricing, price_preference
from gtt_router.model_updates import annotate_updates, new_model_preference
from gtt_router.policy import rank_routes
from gtt_router.store import StateStore
from gtt_router.preferences import parse_preferences


class PricingUpdatesTest(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.store = StateStore(Path(self.folder.name) / 'state.sqlite')
        self.task = Task.parse({'kind': 'extract', 'prompt': 'check', 'token_budget': 150000})

    def tearDown(self):
        self.store.close()
        self.folder.cleanup()

    def route(self, name, cost=None, prior=50, source='a', provider='p'):
        return Route(source, provider, name, auth_hint=True, pricing=normalize_pricing(cost, 'test'),
                     profile={'quality_prior': prior, 'latency_prior_s': 10})

    def test_unknown_and_name_free_are_not_free(self):
        route = self.route('model-free')
        result = price_preference(self.task, route, 2000, 100000)
        self.assertFalse(result['known'])
        self.assertFalse(result['free'])
        self.assertIsNone(result['estimated_request_usd'])
        self.assertEqual(result['score_boost'], 0)

    def test_explicit_free_wins_same_quality_and_cheaper_wins_paid(self):
        free = self.route('free', {'input': 0, 'output': 0})
        cheap = self.route('cheap', {'input': .1, 'output': .2})
        expensive = self.route('expensive', {'input': 5, 'output': 20})
        plans, _ = rank_routes(self.task, [expensive, cheap, free], {}, self.store)
        self.assertEqual([p['route'].model for p in plans], ['free', 'cheap', 'expensive'])
        self.assertGreater(plans[0]['reasons']['free_parallel_boost'], 0)

    def test_free_cannot_bypass_quality(self):
        task = Task.parse({'kind': 'logic_check', 'prompt': 'review'})
        free = self.route('free', {'input': 0, 'output': 0}, prior=20)
        plans, rejected = rank_routes(task, [free], {}, self.store)
        self.assertFalse(plans)
        self.assertIn('quality_floor_not_met', rejected[0]['reasons'])

    def test_stale_price_negative_price_and_currency_are_unknown(self):
        r = self.route('r', {'input': 0, 'output': 0})
        r.pricing['captured_at'] = now_ms() - 86400001
        self.assertFalse(price_preference(self.task, r, 1, 1)['known'])
        for bad in [{'input': -1, 'output': 0}, {'input': 0, 'output': 0, 'currency': 'EUR'}, {'input': True, 'output': 0}]:
            self.assertEqual(normalize_pricing(bad, 'test'), {})

    def test_uncached_estimate_and_large_context_tier(self):
        r = self.route('r', {'input': 1, 'output': 2, 'cache_read': 0,
                             'context_over_200k': {'input': 3, 'output': 4}})
        p = price_preference(self.task, r, 1_000_000, 1_000_000)
        self.assertEqual(p['estimated_request_usd'], 7)

    def test_exact_new_version_separate_providers_and_editions(self):
        old = self.route('muse-spark-1.2-contributor')
        new = self.route('muse-spark-1.3-contributor')
        other_provider = self.route('muse-spark-1.9-contributor', provider='other')
        other_edition = self.route('muse-spark-2.0', provider='p')
        annotate_updates([old, new, other_provider, other_edition])
        self.assertFalse(old.catalog_metadata['latest_in_family'])
        self.assertTrue(new.catalog_metadata['latest_in_family'])
        self.assertEqual(old.catalog_metadata['newer_route_ids'], [new.id])
        self.assertEqual(new_model_preference(self.task, other_provider, self.store), 0)
        self.assertNotIn('intelligence', new.profile)

    def test_verified_new_version_boost_and_round_disable(self):
        old = self.route('muse-spark-1.2-contributor')
        new = self.route('muse-spark-1.3-contributor')
        annotate_updates([old, new])
        self.assertEqual(new_model_preference(self.task, new, self.store), 1)
        self.store.recovered(['route:' + new.id])
        self.assertEqual(new_model_preference(self.task, new, self.store), 5)
        plans, _ = rank_routes(self.task, [old, new], {}, self.store, preferences={'prefer_new_models': False})
        self.assertTrue(all(p['reasons']['newer_model_boost'] == 0 for p in plans))

    def test_task_window_and_price_inputs(self):
        self.assertEqual(self.task.timeout_mode, 'activity')
        for fields in [{'timeout_mode': 'wrong'}, {'price_weight': float('nan')}, {'prefer_free': 1}, {'hard_deadline_s': 5, 'deadline_s': 600}]:
            with self.assertRaises(ValueError):
                Task.parse({'kind': 'extract', 'prompt': 'x', **fields})

    def test_source_only_scope_preserves_explicit_task_price_controls(self):
        self.task.price_weight = 0
        self.task.prefer_free = False
        route = self.route('r', {'input': 0, 'output': 0})
        scope = parse_preferences({'preferred_sources': ['a']})
        price = price_preference(self.task, route, 1000, 32000, scope)
        self.assertEqual(price['score_boost'], 0)
        plans, _ = rank_routes(self.task, [route], {}, self.store, preferences=scope)
        self.assertEqual(plans[0]['reasons']['free_parallel_boost'], 0)

    def test_cached_fees_and_overflow_never_appear_as_free(self):
        for rates in [{'input': 0, 'output': 0, 'cache_read': 1}, {'input': 1e308, 'output': 1e308}]:
            route = self.route('r', rates)
            price = price_preference(self.task, route, 1000000, 1000000)
            self.assertFalse(price['free'])
            self.assertTrue(price['estimated_request_usd'] is None or price['estimated_request_usd'] < float('inf'))

    def test_broad_native_family_and_equivalent_version_do_not_create_false_upgrade(self):
        routes = [self.route(x) for x in ['muse-spark-1.3', 'muse-spark-1.3.0', 'muse-bloom-9.0']]
        for route in routes:
            route.catalog_metadata['family'] = 'muse'
        annotate_updates(routes)
        self.assertTrue(all(r.catalog_metadata['latest_in_family'] for r in routes))
        self.assertTrue(all(not r.catalog_metadata['newer_route_ids'] for r in routes))
        self.assertEqual(routes[2].catalog_metadata['cohort_size'], 1)
        self.assertTrue(all(new_model_preference(self.task, r, self.store) == 0 for r in routes))

    def test_huge_json_integer_is_unknown_price_and_bounded_validation_error(self):
        self.assertEqual(normalize_pricing({'input': 10**1000, 'output': 1}, 'test'), {})
        with self.assertRaises(ValueError):
            Task.parse({'kind': 'extract', 'prompt': 'x', 'price_weight': 10**1000})

    def test_round_can_reenable_new_model_preference(self):
        self.task.prefer_new_models = False
        routes = [self.route(x) for x in ['muse-spark-1.2', 'muse-spark-1.3']]
        annotate_updates(routes)
        plans, _ = rank_routes(self.task, routes, {}, self.store,
                               preferences=parse_preferences({'prefer_new_models': True}))
        newest = next(p for p in plans if p['route'].model.endswith('1.3'))
        self.assertEqual(newest['reasons']['newer_model_boost'], 1)


if __name__ == '__main__':
    unittest.main()
