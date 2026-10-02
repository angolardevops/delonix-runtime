import { NestFactory } from "@nestjs/core";
import type { NestExpressApplication } from "@nestjs/platform-express";
import { SwaggerModule } from "@nestjs/swagger";

import { AppModule } from "./app.module";
import type { AppConfig } from "./config/app-config";
import { accessLogMiddleware } from "./http/access-log";
import { requestIdMiddleware } from "./http/request-id";
import type { AppLogger } from "./logging/app-logger";
import { buildOpenApiDocument } from "./openapi";

/**
 * Builds the application exactly as it runs: main.ts and the e2e tests both
 * call this, so the tests exercise the real middleware order, body parser and
 * error mapping. It does not listen.
 */
export async function createApp(config: AppConfig, logger: AppLogger): Promise<NestExpressApplication> {
  const app = await NestFactory.create<NestExpressApplication>(AppModule.forRoot(config, logger), {
    logger,
    // Keeps the exact bytes of each body (req.rawBody): the inbound webhook
    // signature is computed over them, never over a re-serialised object.
    rawBody: true,
  });
  configureApp(app, config, logger);
  if (config.env === "development") {
    // Interactive documentation, development only (a dev shortcut production refuses).
    SwaggerModule.setup("api/docs", app, buildOpenApiDocument(app));
  }
  return app;
}

function configureApp(app: NestExpressApplication, config: AppConfig, logger: AppLogger): void {
  // Before the body parser: a request with a malformed body still gets an id
  // and an access-log line.
  app.use(requestIdMiddleware);
  app.use(accessLogMiddleware(logger));
  app.useBodyParser("json", { limit: config.maxBodyBytes });
  app.disable("x-powered-by");
  const server = app.getHttpServer();
  server.requestTimeout = config.requestTimeoutMs;
  server.headersTimeout = config.headersTimeoutMs;
  server.keepAliveTimeout = config.keepAliveTimeoutMs;
}
