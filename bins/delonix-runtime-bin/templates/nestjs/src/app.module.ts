import { type DynamicModule, Global, Module } from "@nestjs/common";
import { APP_FILTER, APP_INTERCEPTOR, APP_PIPE } from "@nestjs/core";

import { AppConfig } from "./config/app-config";
import { HealthModule } from "./health/health.module";
import { AllExceptionsFilter } from "./http/all-exceptions.filter";
import { RequestValidationPipe } from "./http/validation";
import { AppLogger } from "./logging/app-logger";
import { NotesModule } from "./notes/notes.module";
import { HttpRouteInterceptor } from "./telemetry/http-route.interceptor";
import { InboundWebhookModule, OutboundWebhookModule } from "./webhooks/webhooks.module";

/** The validated configuration and the logger, available to every module. */
@Global()
@Module({})
class CoreModule {
  static register(config: AppConfig, logger: AppLogger): DynamicModule {
    return {
      module: CoreModule,
      providers: [
        { provide: AppConfig, useValue: config },
        { provide: AppLogger, useValue: logger },
      ],
      exports: [AppConfig, AppLogger],
    };
  }
}

/**
 * The root module. It is built from an already validated configuration, so a
 * module can decide at composition time what exists (the inbound webhook
 * endpoint only when its secret is set).
 */
@Module({})
export class AppModule {
  static forRoot(config: AppConfig, logger: AppLogger): DynamicModule {
    return {
      module: AppModule,
      imports: [
        CoreModule.register(config, logger),
        OutboundWebhookModule,
        HealthModule,
        NotesModule,
        InboundWebhookModule.register(config),
      ],
      providers: [
        { provide: APP_FILTER, useClass: AllExceptionsFilter },
        { provide: APP_PIPE, useClass: RequestValidationPipe },
        { provide: APP_INTERCEPTOR, useClass: HttpRouteInterceptor },
      ],
    };
  }
}
