import { ApiProperty } from "@nestjs/swagger";

import type { Note, NotePage } from "../note";

export class NoteResponse implements Note {
  @ApiProperty({ format: "uuid" })
  id!: string;

  @ApiProperty()
  title!: string;

  @ApiProperty()
  body!: string;

  @ApiProperty({ format: "date-time" })
  created_at!: string;
}

export class NotePageResponse implements NotePage {
  @ApiProperty({ type: [NoteResponse] })
  items!: NoteResponse[];

  @ApiProperty()
  total!: number;

  @ApiProperty()
  offset!: number;

  @ApiProperty()
  limit!: number;
}
