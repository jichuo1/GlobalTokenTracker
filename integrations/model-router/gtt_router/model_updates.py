"""Exact provider/family cohorts; a newer name is not a quality measurement."""

import re
from .models import now_ms, number


def identity(route):
    meta = route.catalog_metadata
    # Dotted numeric versions only; do not fuzzy-merge arbitrary model aliases.
    match = re.fullmatch(r"(.+?)[-_]v?(\d+(?:\.\d+)+)([^0-9].*)?", route.model)
    if not match:
        return None
    prefix, version, edition = match.groups()
    parts = list(map(int, version.split(".")))
    while len(parts) > 1 and parts[-1] == 0:
        parts.pop()
    return (route.source, route.provider, meta.get("family") or prefix, prefix, edition or "", route.effort), tuple(parts)


def annotate_updates(routes):
    cohorts = {}
    for route in routes:
        key = identity(route)
        if key:
            cohorts.setdefault(key[0], []).append((key[1], route))
    for peers in cohorts.values():
        newest = max(version for version, _ in peers)
        distinct_versions = len({version for version, _ in peers})
        for version, route in peers:
            newer = [peer.id for other, peer in peers if other > version]
            route.catalog_metadata.update({"version": list(version), "latest_in_family": version == newest,
                                           "cohort_size": len(peers), "distinct_versions": distinct_versions, "newer_route_ids": newer,
                                           "identity_source": "native_id_exact_version_cohort"})


def new_model_preference(task, route, store, enabled=None):
    enabled = task.prefer_new_models if enabled is None else enabled
    if not enabled or not route.catalog_metadata.get("latest_in_family") or route.catalog_metadata.get("distinct_versions", 0) < 2:
        return 0
    peers = route.catalog_metadata.get("version")
    if not peers:
        return 0
    health = store.health("route:" + route.id)
    verified = number(health.get("last_verified_at"), 0)
    # Candidate exploration is modest. Actual verification earns a larger
    # preference, but quality/capability gates in policy still run first.
    return 5 if verified > 0 and 0 <= now_ms() - verified <= 86400000 else 1
