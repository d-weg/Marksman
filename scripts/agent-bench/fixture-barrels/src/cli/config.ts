import { QuotaPolicy } from "../core/policy";

/** Parse a "name:limit:windowSec" config line into a policy. */
export function parsePolicy(line: string): QuotaPolicy {
  const [name, limit, windowSec] = line.split(":");
  return { name, limit: Number(limit), windowSec: Number(windowSec) };
}

export function parseConfig(text: string): QuotaPolicy[] {
  return text
    .split("\n")
    .filter((l) => l.trim().length > 0)
    .map(parsePolicy);
}
