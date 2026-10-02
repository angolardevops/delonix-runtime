/**
 * Standard Webhooks (https://www.standardwebhooks.com): headers webhook-id,
 * webhook-timestamp and webhook-signature, the signature being
 * base64(HMAC-SHA256(key, "<id>.<timestamp>.<raw body>")) prefixed with "v1,".
 * A published format lets a peer written in another language, or an existing
 * library, talk to this service without reading its code.
 */
import { createHmac, timingSafeEqual } from "node:crypto";

export const HEADER_ID = "webhook-id";
export const HEADER_TIMESTAMP = "webhook-timestamp";
export const HEADER_SIGNATURE = "webhook-signature";

/** How far a timestamp may be from now, either way: bounds replay of a captured request. */
export const TOLERANCE_SECONDS = 5 * 60;

/** Decodes a "whsec_<base64>" secret; the key must be at least 24 bytes (the spec's lower bound). */
export function parseSecret(secret: string): Buffer {
  if (!secret.startsWith("whsec_"))
    throw new Error('webhook secret must look like "whsec_<base64>"');
  const raw = secret.slice("whsec_".length);
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(raw)) throw new Error("webhook secret is not valid base64");
  const key = Buffer.from(raw, "base64");
  if (key.length < 24) throw new Error("webhook secret must decode to at least 24 bytes");
  return key;
}

/** The webhook-signature header value. */
export function sign(key: Buffer, id: string, timestampSeconds: number, body: Buffer): string {
  const mac = createHmac("sha256", key);
  mac.update(`${id}.${timestampSeconds}.`);
  mac.update(body);
  return "v1," + mac.digest("base64");
}

export type VerifyFailure = "missing_headers" | "bad_timestamp" | "bad_signature";

/**
 * Verifies a delivery over the RAW body bytes (never a re-encoded body:
 * re-serializing JSON changes bytes and breaks every signature). Returns the
 * failure, or undefined when the delivery is authentic.
 */
export function verify(
  key: Buffer,
  id: string | undefined,
  timestamp: string | undefined,
  signatures: string | undefined,
  body: Buffer,
  nowSeconds: number,
): VerifyFailure | undefined {
  if (!id || !timestamp || !signatures) return "missing_headers";
  if (!/^\d{1,12}$/.test(timestamp)) return "bad_timestamp";
  const ts = Number(timestamp);
  if (Math.abs(nowSeconds - ts) > TOLERANCE_SECONDS) return "bad_timestamp";
  const want = Buffer.from(sign(key, id, ts, body));
  // Several space-separated signatures are allowed (key rotation).
  for (const candidate of signatures.split(/\s+/)) {
    const got = Buffer.from(candidate);
    if (got.length === want.length && timingSafeEqual(got, want)) return undefined;
  }
  return "bad_signature";
}
