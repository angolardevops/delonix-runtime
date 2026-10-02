import { ApiProperty, ApiPropertyOptional } from "@nestjs/swagger";

import { CreateNoteDto } from "../../notes/dto/create-note.dto";

/** Documentation of an inbound event; the controller parses the raw bytes itself. */
export class WebhookEventDto {
  @ApiProperty({ enum: ["ping", "note.create"] })
  type!: "ping" | "note.create";

  @ApiPropertyOptional({ type: CreateNoteDto, description: "Required for note.create." })
  data?: CreateNoteDto;
}
