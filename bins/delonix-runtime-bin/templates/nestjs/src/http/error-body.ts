import { ApiProperty, ApiPropertyOptional } from "@nestjs/swagger";

export class ErrorDetail {
  @ApiProperty({
    example: "validation_failed",
    description:
      "validation_failed (422), malformed_json (400), body_too_large (413), not_found (404), internal (500), and the webhook codes missing_signature (400), invalid_signature (401), unknown_event (422).",
  })
  code!: string;

  @ApiProperty({ example: "invalid input" })
  message!: string;

  @ApiPropertyOptional({
    type: "object",
    additionalProperties: { type: "string" },
    example: { title: "is required" },
  })
  fields?: Record<string, string>;

  @ApiProperty({ example: "4f1c2d3e4b5a6978" })
  request_id!: string;
}

/** The one error shape every endpoint answers with. */
export class ErrorBody {
  @ApiProperty({ type: ErrorDetail })
  error!: ErrorDetail;
}

/**
 * An error the transport answers as-is: status, code, message, fields. Thrown
 * by the validation pipe and the inbound webhook; mapped by AllExceptionsFilter.
 */
export class HttpProblem extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly fields?: Record<string, string>,
  ) {
    super(message);
    this.name = "HttpProblem";
  }
}
