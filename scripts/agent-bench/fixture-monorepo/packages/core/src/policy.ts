/** Retry policies shared by every service in the workspace. */

export interface RetryPolicy {
  maxAttempts: number;
  backoffMs: number;
}

export function defaultRetry(): RetryPolicy {
  return { maxAttempts: 3, backoffMs: 250 };
}

/** Exponential backoff delay before the given (0-based) attempt. */
export function nextDelay(policy: RetryPolicy, attempt: number): number {
  return policy.backoffMs * Math.pow(2, attempt);
}
