// Standard Webhooks (https://www.standardwebhooks.com): headers webhook-id,
// webhook-timestamp and webhook-signature, the signature being
// base64(HMAC-SHA256(key, "<id>.<timestamp>.<raw body>")) prefixed with "v1,".
// A published format lets a peer written in another language, or an existing
// library, talk to this service without reading its code.
import "server-only";
import { createHmac, timingSafeEqual } from "node:crypto";

export const HEADER_ID = "webhook-id";
export const HEADER_TIMESTAMP = "webhook-timestamp";
export const HEADER_SIGNATURE = "webhook-signature";

/** How far a timestamp may be from now, either way: bounds replay of a captured request. */
export const TOLERANCE_SECONDS = 5 * 60;

/** Decodes "whsec_<base64>"; the key must be at least 24 bytes (the spec's lower bound). */
export function parseSecret(secret: string): Buffer {
  if (!secret.startsWith("whsec_")) {
    throw new Error('webhook secret must look like "whsec_<base64>"');
  }
  const raw = secret.slice("whsec_".length);
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(raw) || raw.length % 4 !== 0) {
    throw new Error("webhook secret is not valid base64");
  }
  const key = Buffer.from(raw, "base64");
  if (key.length < 24) {
    throw new Error("webhook secret must decode to at least 24 bytes");
  }
  return key;
}

/** The webhook-signature header value for one delivery. */
export function sign(key: Buffer, id: string, timestampSeconds: number, body: Uint8Array | string): string {
  const mac = createHmac("sha256", key);
  mac.update(`${id}.${timestampSeconds}.`);
  mac.update(body);
  return "v1," + mac.digest("base64");
}

export type VerifyResult = "ok" | "missing_headers" | "bad_timestamp" | "bad_signature";

/**
 * Checks a delivery over the RAW body bytes — never a re-serialized body:
 * re-encoding JSON changes bytes and breaks every signature. Distinct results
 * let the handler answer 400 for a malformed request and 401 for a bad
 * signature, without saying which byte was wrong.
 */
export function verify(
  key: Buffer,
  headers: { id: string | null; timestamp: string | null; signature: string | null },
  body: Uint8Array,
  nowSeconds: number,
): VerifyResult {
  const { id, timestamp, signature } = headers;
  if (!id || !timestamp || !signature) return "missing_headers";
  if (!/^\d{1,12}$/.test(timestamp)) return "bad_timestamp";
  const ts = Number(timestamp);
  if (Math.abs(nowSeconds - ts) > TOLERANCE_SECONDS) return "bad_timestamp";
  const expected = Buffer.from(sign(key, id, ts, body));
  // Several space-separated signatures are allowed (key rotation).
  for (const candidate of signature.split(" ").filter(Boolean)) {
    const got = Buffer.from(candidate);
    if (got.length === expected.length && timingSafeEqual(got, expected)) return "ok";
  }
  return "bad_signature";
}
