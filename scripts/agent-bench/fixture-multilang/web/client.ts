import { formatLatency, formatPercent } from "./format";

export interface DashboardRow {
  route: string;
  p50: string;
  p99: string;
  errorRate: string;
}

export function renderRow(
  route: string,
  p50Ms: number,
  p99Ms: number,
  errors: number,
  total: number,
): DashboardRow {
  return {
    route,
    p50: formatLatency(p50Ms),
    p99: formatLatency(p99Ms),
    errorRate: formatPercent(total === 0 ? 0 : errors / total),
  };
}
