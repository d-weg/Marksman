import { RetryPolicy, defaultRetry, nextDelay } from "@acme/core";

const aggressive: RetryPolicy = { maxAttempts: 6, backoffMs: 50 };

export function policyForRoute(route: string): RetryPolicy {
  if (route.startsWith("/payments")) return aggressive;
  return defaultRetry();
}

export function retryAfter(route: string, attempt: number): number {
  return nextDelay(policyForRoute(route), attempt);
}
