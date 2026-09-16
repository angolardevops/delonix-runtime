import { NextResponse } from "next/server";

// Liveness probe (used by the Delonixfile HEALTHCHECK).
export function GET() {
  return NextResponse.json({ status: "alive" });
}
