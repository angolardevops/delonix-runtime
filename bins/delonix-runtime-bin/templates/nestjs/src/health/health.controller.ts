import { Controller, Get, HttpStatus, Res } from "@nestjs/common";
import { ApiOkResponse, ApiOperation, ApiServiceUnavailableResponse, ApiTags } from "@nestjs/swagger";
import type { Response } from "express";

import { AppLogger } from "../logging/app-logger";
import { StatusBody } from "./status-body";
import { ReadinessService } from "./readiness.service";

@ApiTags("health")
@Controller("api/v1/health")
export class HealthController {
  constructor(
    private readonly readiness: ReadinessService,
    private readonly logger: AppLogger,
  ) {}

  @Get("live")
  @ApiOperation({ summary: "Liveness — the process answers." })
  @ApiOkResponse({ description: "Alive.", type: StatusBody })
  live(): StatusBody {
    return { status: "alive" };
  }

  @Get("ready")
  @ApiOperation({ summary: "Readiness — the process should receive traffic." })
  @ApiOkResponse({ description: "Ready.", type: StatusBody })
  @ApiServiceUnavailableResponse({
    description: "Starting, draining, or a required dependency is failing.",
    type: StatusBody,
  })
  async ready(@Res({ passthrough: true }) res: Response): Promise<StatusBody> {
    const { state, failing } = await this.readiness.evaluate();
    if (state !== "ready") {
      res.status(HttpStatus.SERVICE_UNAVAILABLE);
      return { status: state };
    }
    if (failing !== undefined) {
      this.logger.warn("readiness dependency failing", { dependency: failing });
      res.status(HttpStatus.SERVICE_UNAVAILABLE);
      return { status: "unavailable", dependency: failing };
    }
    return { status: "ready" };
  }
}
