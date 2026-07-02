/** Rate-limit policies: how many requests a caller may make per time window. */

export interface QuotaPolicy {
  name: string;
  limit: number;
  windowSec: number;
}

export function defaultPolicy(name: string): QuotaPolicy {
  return { name, limit: 100, windowSec: 60 };
}

export function isStricter(a: QuotaPolicy, b: QuotaPolicy): boolean {
  return a.limit / a.windowSec < b.limit / b.windowSec;
}
