import { RetryPolicy } from "@acme/core";

export interface DigestJob {
  name: string;
  policy: RetryPolicy;
}

export function nightly(): DigestJob {
  return { name: "nightly", policy: { maxAttempts: 1, backoffMs: 1000 } };
}

export function describe(job: DigestJob): string {
  return `${job.name}: up to ${job.policy.maxAttempts} attempts`;
}
