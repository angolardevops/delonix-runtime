import { ApiProperty, ApiPropertyOptional } from "@nestjs/swagger";
import { IsOptional, IsString } from "class-validator";

import { MAX_BODY_LENGTH, MAX_TITLE_LENGTH } from "../note";

/**
 * The body of POST /api/v1/notes. The DTO checks the shape (strings, no
 * unknown properties); the length rules are the use case's, so the inbound
 * webhook path enforces the same ones.
 */
export class CreateNoteDto {
  @ApiProperty({ minLength: 1, maxLength: MAX_TITLE_LENGTH, description: "Trimmed; 1–200 characters." })
  @IsOptional()
  @IsString({ message: "must be a string" })
  title?: string;

  @ApiPropertyOptional({ maxLength: MAX_BODY_LENGTH })
  @IsOptional()
  @IsString({ message: "must be a string" })
  body?: string;
}
