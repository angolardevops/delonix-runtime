import { ApiProperty, ApiPropertyOptional } from "@nestjs/swagger";

export class StatusBody {
  @ApiProperty({ example: "ready" })
  status!: string;

  @ApiPropertyOptional({ description: "The failing dependency, when there is one." })
  dependency?: string;
}
