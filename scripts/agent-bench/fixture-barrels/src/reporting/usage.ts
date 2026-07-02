import { QuotaPolicy, windowStart } from "../core";

export interface UsageRow {
  policy: string;
  window: number;
  used: number;
}

/** The internal policy applied to the reporting endpoints themselves. */
export function reportingPolicy(): QuotaPolicy {
  return { name: "reporting", limit: 5, windowSec: 300 };
}

export function headroom(policy: QuotaPolicy, row: UsageRow): number {
  return Math.max(0, policy.limit - row.used);
}

export function summarize(rows: UsageRow[], now: number): string {
  const current = windowStart(now, 60);
  const live = rows.filter((r) => r.window === current);
  return live.map((r) => `${r.policy}: ${r.used}`).join("\n");
}
