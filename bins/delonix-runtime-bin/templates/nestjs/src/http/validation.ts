import { type ArgumentMetadata, Injectable, ValidationPipe, type ValidationError } from "@nestjs/common";

import { HttpProblem } from "./error-body";

/**
 * The global ValidationPipe: DTO classes decide the shape (types, allowed
 * properties), and a failure is answered in the contract's error shape.
 * Unknown properties are refused (whitelist + forbidNonWhitelisted). The
 * business rules (lengths, ranges) stay in the use case.
 */
@Injectable()
export class RequestValidationPipe extends ValidationPipe {
  constructor() {
    super({
      whitelist: true,
      forbidNonWhitelisted: true,
      transform: true,
      exceptionFactory: (errors: ValidationError[]) =>
        new HttpProblem(422, "validation_failed", "invalid input", fieldsOf(errors)),
    });
  }

  override transform(value: unknown, metadata: ArgumentMetadata): Promise<unknown> {
    // Valid JSON that is not an object (`[1]`, `"x"`, `null`) is not the
    // expected body: answer as malformed, not as a list of odd field errors.
    if (metadata.type === "body" && (value === null || typeof value !== "object" || Array.isArray(value))) {
      throw new HttpProblem(400, "malformed_json", "request body is not the expected JSON");
    }
    return super.transform(value, metadata) as Promise<unknown>;
  }
}

function fieldsOf(errors: ValidationError[]): Record<string, string> {
  const fields: Record<string, string> = {};
  for (const error of errors) {
    const constraints = error.constraints ?? {};
    if ("whitelistValidation" in constraints) {
      fields[error.property] = "is not allowed";
    } else {
      fields[error.property] = Object.values(constraints)[0] ?? "is invalid";
    }
  }
  return fields;
}
