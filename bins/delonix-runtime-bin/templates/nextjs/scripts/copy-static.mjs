// `next build` with output: "standalone" writes .next/standalone/server.js but
// leaves the browser assets outside it (they are meant for a CDN). This copies
// them next to the server so `pnpm start` and the image serve the page's own
// JavaScript and CSS. The Delonixfile does the same copy with COPY.
import { cpSync, existsSync } from "node:fs";

if (!existsSync(".next/standalone")) {
  console.error(
    'copy-static: .next/standalone is missing — is output: "standalone" still set in next.config.ts?',
  );
  process.exit(1);
}
cpSync(".next/static", ".next/standalone/.next/static", { recursive: true });
if (existsSync("public")) cpSync("public", ".next/standalone/public", { recursive: true });
console.log("copy-static: .next/static → .next/standalone/.next/static");
