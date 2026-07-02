export function nowSec(): number {
  return Math.floor(Date.now() / 1000);
}

/** Start of the window containing `now`, aligned to windowSec boundaries. */
export function windowStart(now: number, windowSec: number): number {
  return now - (now % windowSec);
}
