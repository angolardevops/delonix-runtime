import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { testApp } from "./helpers.js";

/**
 * The contract drift gate: api/openapi.json is the checked-in copy of what the
 * route schemas generate. Change a route or a schema and this fails until
 * `pnpm openapi:write` regenerates the file — in the same change.
 */
test("api/openapi.json is what the routes generate", async () => {
  const { app } = await testApp();
  await app.ready();
  const generated = JSON.parse(JSON.stringify(app.swagger()));
  const checkedIn = JSON.parse(readFileSync("api/openapi.json", "utf8"));
  assert.deepEqual(generated, checkedIn, "contract drift: run `pnpm openapi:write`");
  await app.close();
});
