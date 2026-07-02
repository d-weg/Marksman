import { allow } from "./middleware/throttle";
import { setOverride, getOverride } from "./api/admin";
import { reportingPolicy, summarize } from "./reporting/usage";
import { parseConfig } from "./cli/config";
import { nowSec } from "./core";

export function boot(configText: string): void {
  for (const policy of parseConfig(configText)) {
    setOverride(policy.name, policy.limit, policy.windowSec);
  }
}

export function handle(caller: string | undefined): string {
  if (!allow(caller)) return "429";
  return "200";
}

export function report(): string {
  const policy = getOverride("reporting") ?? reportingPolicy();
  return `limit=${policy.limit}\n${summarize([], nowSec())}`;
}
