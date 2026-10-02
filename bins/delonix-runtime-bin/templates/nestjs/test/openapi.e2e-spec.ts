import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { buildOpenApiDocument } from "../src/openapi";
import { startTestApp, TEST_SECRET, type TestApp } from "./support/app";

const CONTRACT = join(__dirname, "..", "api", "openapi.json");

/**
 * The contract drift gate. api/openapi.json is the checked-in contract; the
 * document generated from the controllers and DTOs must equal it. Adding,
 * removing or changing a route, a parameter, a response or a schema without
 * regenerating the file (`pnpm openapi:write`) fails here — and so does
 * editing the file by hand.
 */
describe("OpenAPI contract", () => {
  let t: TestApp;
  let generated: Record<string, unknown>;

  beforeAll(async () => {
    // With the inbound secret set, so the optional endpoint is part of the contract.
    t = await startTestApp({ WEBHOOK_INBOUND_SECRET: TEST_SECRET });
    generated = JSON.parse(JSON.stringify(buildOpenApiDocument(t.app))) as Record<string, unknown>;
    if (process.env.UPDATE_OPENAPI === "1") {
      writeFileSync(CONTRACT, JSON.stringify(generated, null, 2) + "\n");
    }
  });

  afterAll(async () => {
    await t.close();
  });

  it("api/openapi.json equals the document generated from the code", () => {
    const checkedIn = JSON.parse(readFileSync(CONTRACT, "utf8")) as Record<string, unknown>;
    expect(generated).toEqual(checkedIn);
  });
});
