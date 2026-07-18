import { Controller, Get } from "@nestjs/common";

/** Liveness/readiness probes (used by the Delonixfile HEALTHCHECK). */
@Controller("api/v1/health")
export class HealthController {
  @Get("live")
  live(): { status: string } {
    return { status: "alive" };
  }

  @Get("ready")
  ready(): { status: string } {
    return { status: "ready" };
  }
}
