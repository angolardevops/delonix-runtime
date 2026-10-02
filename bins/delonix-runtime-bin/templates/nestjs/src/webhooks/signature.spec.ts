import { createHmac } from "node:crypto";

import { DeliveryDedup } from "./dedup";
import { parseSecret, sign, TOLERANCE_SECONDS, verify } from "./signature";

const key = Buffer.alloc(32, 9);
const body = Buffer.from('{"type":"ping"}');
const NOW = 1_800_000_000;

describe("sign / verify", () => {
  it("matches the Standard Webhooks definition", () => {
    const expected = createHmac("sha256", key).update(`msg_1.${NOW}.{"type":"ping"}`).digest("base64");
    expect(sign(key, "msg_1", NOW, body)).toBe(`v1,${expected}`);
  });

  it("accepts its own signature inside the window", () => {
    const signature = sign(key, "msg_1", NOW, body);
    expect(verify(key, "msg_1", String(NOW), signature, body, NOW)).toBe("ok");
    expect(verify(key, "msg_1", String(NOW), signature, body, NOW + TOLERANCE_SECONDS)).toBe("ok");
    expect(verify(key, "msg_1", String(NOW), signature, body, NOW - TOLERANCE_SECONDS)).toBe("ok");
  });

  it("refuses outside the replay window, either way", () => {
    const signature = sign(key, "msg_1", NOW, body);
    expect(verify(key, "msg_1", String(NOW), signature, body, NOW + TOLERANCE_SECONDS + 1)).toBe(
      "bad_timestamp",
    );
    expect(verify(key, "msg_1", String(NOW), signature, body, NOW - TOLERANCE_SECONDS - 1)).toBe(
      "bad_timestamp",
    );
    expect(verify(key, "msg_1", "yesterday", signature, body, NOW)).toBe("bad_timestamp");
  });

  it("refuses a changed body, id, timestamp or key", () => {
    const signature = sign(key, "msg_1", NOW, body);
    expect(verify(key, "msg_1", String(NOW), signature, Buffer.from('{"type":"ping" }'), NOW)).toBe(
      "bad_signature",
    );
    expect(verify(key, "msg_2", String(NOW), signature, body, NOW)).toBe("bad_signature");
    expect(verify(key, "msg_1", String(NOW + 1), signature, body, NOW)).toBe("bad_signature");
    expect(verify(Buffer.alloc(32, 8), "msg_1", String(NOW), signature, body, NOW)).toBe("bad_signature");
    expect(verify(key, "msg_1", String(NOW), "v1,short", body, NOW)).toBe("bad_signature");
  });

  it("reports missing headers", () => {
    expect(verify(key, undefined, String(NOW), "v1,x", body, NOW)).toBe("missing_headers");
    expect(verify(key, "msg_1", "", "v1,x", body, NOW)).toBe("missing_headers");
    expect(verify(key, "msg_1", String(NOW), undefined, body, NOW)).toBe("missing_headers");
  });
});

describe("parseSecret", () => {
  it("decodes whsec_<base64>", () => {
    expect(parseSecret("whsec_" + key.toString("base64"))).toEqual(key);
  });

  it("refuses a missing prefix and a short key", () => {
    expect(() => parseSecret(key.toString("base64"))).toThrow(/whsec_/);
    expect(() => parseSecret("whsec_" + Buffer.alloc(8).toString("base64"))).toThrow(/24 bytes/);
  });
});

describe("DeliveryDedup", () => {
  it("sees an id once inside the window and again after it", () => {
    const dedup = new DeliveryDedup();
    expect(dedup.firstSeen("a", NOW)).toBe(true);
    expect(dedup.firstSeen("a", NOW + 10)).toBe(false);
    expect(dedup.firstSeen("a", NOW + 2 * TOLERANCE_SECONDS + 1)).toBe(true);
  });

  it("forgets on request and evicts the oldest when full", () => {
    const dedup = new DeliveryDedup(2);
    dedup.firstSeen("a", NOW);
    dedup.forget("a");
    expect(dedup.firstSeen("a", NOW)).toBe(true);
    dedup.firstSeen("b", NOW);
    dedup.firstSeen("c", NOW);
    expect(dedup.firstSeen("a", NOW)).toBe(true);
  });
});
