import { ApiPropertyOptional } from "@nestjs/swagger";
import { Transform } from "class-transformer";
import { IsInt, IsOptional } from "class-validator";

import { DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE } from "../note";

/** "" and absent mean the default; anything else must be an integer. */
const toNumber = ({ value }: { value: unknown }): unknown =>
  value === undefined || value === "" ? undefined : Number(value);

export class ListNotesQuery {
  @ApiPropertyOptional({ minimum: 0, default: 0 })
  @IsOptional()
  @Transform(toNumber)
  @IsInt({ message: "must be an integer" })
  offset?: number;

  @ApiPropertyOptional({ minimum: 1, maximum: MAX_PAGE_SIZE, default: DEFAULT_PAGE_SIZE })
  @IsOptional()
  @Transform(toNumber)
  @IsInt({ message: "must be an integer" })
  limit?: number;
}
