"""Nightly rollup: fold per-minute latency buckets into daily aggregates."""


def rollup_day(buckets):
    total = sum(b["count"] for b in buckets)
    worst = max((b["p99"] for b in buckets), default=0.0)
    return {"count": total, "p99": worst}
