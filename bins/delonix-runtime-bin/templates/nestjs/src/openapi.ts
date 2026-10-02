import type { INestApplication } from "@nestjs/common";
import { DocumentBuilder, type OpenAPIObject, SwaggerModule } from "@nestjs/swagger";

/**
 * The OpenAPI document generated from the controllers and DTOs. The checked-in
 * copy (api/openapi.json) is compared with it by test/openapi.e2e-spec.ts:
 * a route or schema changed without regenerating the file fails the tests.
 * Regenerate with `pnpm openapi:write`.
 */
export function buildOpenApiDocument(app: INestApplication): OpenAPIObject {
  const info = new DocumentBuilder()
    .setTitle("__NAME__")
    .setDescription(
      "HTTP contract of __NAME__. Every error answers " +
        '{"error":{"code","message","fields"?,"request_id"}}. Breaking changes go to a new /api/v2 prefix.',
    )
    .setVersion("1.0.0")
    .build();
  return SwaggerModule.createDocument(app, info);
}
