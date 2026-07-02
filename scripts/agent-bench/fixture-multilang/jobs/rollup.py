"""Nightly aggregation: fold per-minute latency buckets into larger windows."""


def rollup_day(buckets):
    total = sum(b["count"] for b in buckets)
    worst = max((b["p99"] for b in buckets), default=0.0)
    return {"count": total, "p99": worst}


def rollup_week(buckets_by_day):
    days = [rollup_day(day) for day in buckets_by_day]
    return {
        "count": sum(d["count"] for d in days),
        "p99": max((d["p99"] for d in days), default=0.0),
    }
