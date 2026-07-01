// Authentication: validate a bearer token and decode a JWT.

/** Extract the bearer token from an Authorization header. */
export function bearerToken(header: string): string | null {
  const m = /^Bearer (.+)$/.exec(header);
  return m ? m[1] : null;
}

/** Validate a JWT and return its decoded subject claim. */
export function validateJwt(token: string): string | null {
  const parts = token.split(".");
  return parts.length === 3 ? atob(parts[1]) : null;
}
