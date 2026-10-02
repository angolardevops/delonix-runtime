// Standard Webhooks (https://www.standardwebhooks.com): headers webhook-id,
// webhook-timestamp and webhook-signature, the signature being
// base64(HMAC-SHA256(key, "<id>.<timestamp>.<raw body>")) prefixed with "v1,".
// A published format lets a peer written in another language, or an existing
// library, talk to this service without reading its code.
import { createHmac, timingSafeEqual } from "node:crypto";

export const HEADER_ID = "webhook-id";
export const HEADER_TIMESTAMP = "webhook-timestamp";
export const HEADER_SIGNATURE = "webhook-signature";

/** How far a timestamp may be from now, either way: bounds how long a captured request can be replayed. */
export const TOLERANCE_SECONDS = 5 * 60;

/** Decodes `whsec_<base64>`; the key must be at least 24 bytes. */
export function parseSecret(secret: string): Buffer {
  if (!secret.startsWith("whsec_")) {
    throw new Error('webhook secret must look like "whsec_<base64>"');
  }
  const key = Buffer.from(secret.slice("whsec_".length), "base64");
  if (key.length < 24) {
    throw new Error("webhook secret must decode to at least 24 bytes");
  }
  return key;
}

/** The webhook-signature header value for `body` (the exact bytes sent). */
export function sign(key: Buffer, id: string, timestampSeconds: number, body: Buffer): string {
  const mac = createHmac("sha256", key);
  mac.update(`${id}.${timestampSeconds}.`);
  mac.update(body);
  return `v1,${mac.digest("base64")}`;
}

export type VerifyResult = "ok" | "missing_headers" | "bad_timestamp" | "bad_signature";

/**
 * Checks a delivery over the RAW body bytes — never a re-serialised body:
 * re-encoding JSON changes bytes and breaks every signature. The result says
 * which class of failure it was, never which byte.
 */
export function verify(
  key: Buffer,
  id: string | undefined,
  timestamp: string | undefined,
  signatures: string | undefined,
  body: Buffer,
  nowSeconds: number,
): VerifyResult {
  if (!id || !timestamp || !signatures) {
    return "missing_headers";
  }
  if (!/^\d{1,12}$/.test(timestamp)) {
    return "bad_timestamp";
  }
  const ts = Number(timestamp);
  if (Math.abs(nowSeconds - ts) > TOLERANCE_SECONDS) {
    return "bad_timestamp";
  }
  const expected = Buffer.from(sign(key, id, ts, body));
  // Several space-separated signatures are allowed (key rotation).
  for (const candidate of signatures.split(" ")) {
    const given = Buffer.from(candidate);
    if (given.length === expected.length && timingSafeEqual(given, expected)) {
      return "ok";
    }
  }
  return "bad_signature";
}
