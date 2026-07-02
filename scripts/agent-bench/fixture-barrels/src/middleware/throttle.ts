import { QuotaPolicy, defaultPolicy, nowSec, windowStart } from "../core";

interface Counter {
  start: number;
  count: number;
}

const counters = new Map<string, Counter>();

const anonymous: QuotaPolicy = { name: "anonymous", limit: 20, windowSec: 60 };

export function policyFor(caller: string | undefined): QuotaPolicy {
  if (!caller) return anonymous;
  return defaultPolicy(caller);
}

export function allow(caller: string | undefined): boolean {
  const policy = policyFor(caller);
  const now = nowSec();
  const start = windowStart(now, policy.windowSec);
  const key = `${policy.name}:${start}`;
  const c = counters.get(key) ?? { start, count: 0 };
  c.count += 1;
  counters.set(key, c);
  return c.count <= policy.limit;
}
