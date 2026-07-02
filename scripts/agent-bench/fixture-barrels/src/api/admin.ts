import { QuotaPolicy } from "../core";

const overrides = new Map<string, QuotaPolicy>();

export function setOverride(name: string, limit: number, windowSec: number): QuotaPolicy {
  const policy: QuotaPolicy = { name, limit, windowSec };
  overrides.set(name, policy);
  return policy;
}

export function getOverride(name: string): QuotaPolicy | undefined {
  return overrides.get(name);
}

export function clearOverride(name: string): boolean {
  return overrides.delete(name);
}
