import { Body, Controller, Get, HttpCode, Param, Post, Query, Res } from "@nestjs/common";
import {
  ApiBadRequestResponse,
  ApiCreatedResponse,
  ApiNotFoundResponse,
  ApiOkResponse,
  ApiOperation,
  ApiPayloadTooLargeResponse,
  ApiTags,
  ApiUnprocessableEntityResponse,
} from "@nestjs/swagger";
import type { Response } from "express";

import { ErrorBody } from "../http/error-body";
import { CreateNoteDto } from "./dto/create-note.dto";
import { ListNotesQuery } from "./dto/list-notes.query";
import { NotePageResponse, NoteResponse } from "./dto/note.response";
import { DEFAULT_PAGE_SIZE, type Note, type NotePage } from "./note";
import { NotesService } from "./notes.service";

/** HTTP transport of the notes capability. No rule lives here: it maps HTTP to use cases and back. */
@ApiTags("notes")
@Controller("api/v1/notes")
export class NotesController {
  constructor(private readonly notes: NotesService) {}

  @Post()
  @ApiOperation({ summary: "Create a note." })
  @HttpCode(201)
  @ApiCreatedResponse({ description: "Created. The Location header points at the note.", type: NoteResponse })
  @ApiBadRequestResponse({ description: "Malformed JSON.", type: ErrorBody })
  @ApiPayloadTooLargeResponse({ description: "Body over HTTP_MAX_BODY_BYTES.", type: ErrorBody })
  @ApiUnprocessableEntityResponse({ description: "Invalid input.", type: ErrorBody })
  async create(@Body() dto: CreateNoteDto, @Res({ passthrough: true }) res: Response): Promise<Note> {
    const note = await this.notes.create({ title: dto.title ?? "", body: dto.body });
    res.setHeader("Location", `/api/v1/notes/${note.id}`);
    return note;
  }

  @Get()
  @ApiOperation({ summary: "List notes, newest first." })
  @ApiOkResponse({ description: "One page.", type: NotePageResponse })
  @ApiUnprocessableEntityResponse({ description: "Invalid offset or limit.", type: ErrorBody })
  list(@Query() query: ListNotesQuery): Promise<NotePage> {
    return this.notes.list(query.offset ?? 0, query.limit ?? DEFAULT_PAGE_SIZE);
  }

  @Get(":id")
  @ApiOperation({ summary: "Get one note." })
  @ApiOkResponse({ description: "The note.", type: NoteResponse })
  @ApiNotFoundResponse({ description: "No note with that id.", type: ErrorBody })
  get(@Param("id") id: string): Promise<Note> {
    return this.notes.get(id);
  }
}
