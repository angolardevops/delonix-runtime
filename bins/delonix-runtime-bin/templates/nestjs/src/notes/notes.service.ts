import { randomUUID } from "node:crypto";

import { Injectable } from "@nestjs/common";

import {
  characterCount,
  type CreateNoteInput,
  MAX_BODY_LENGTH,
  MAX_PAGE_SIZE,
  MAX_TITLE_LENGTH,
  type Note,
  NoteNotFoundError,
  type NotePage,
  NoteValidationError,
} from "./note";
import { NotePublisher } from "./note.publisher";
import { NoteRepository } from "./note.repository";

/**
 * The use cases of the notes capability. The rules live here, not in the
 * DTOs: the inbound webhook runs the same `create` as the HTTP API, and both
 * must refuse the same input. The only framework import is `@Injectable`
 * (Nest's DI marker); transport, telemetry and storage are out of reach.
 */
@Injectable()
export class NotesService {
  constructor(
    private readonly repository: NoteRepository,
    private readonly publisher: NotePublisher,
  ) {}

  async create(input: CreateNoteInput): Promise<Note> {
    const title = (input.title ?? "").trim();
    const body = input.body ?? "";
    const fields: Record<string, string> = {};
    if (title === "") {
      fields.title = "is required";
    } else if (characterCount(title) > MAX_TITLE_LENGTH) {
      fields.title = `must be at most ${MAX_TITLE_LENGTH} characters`;
    }
    if (characterCount(body) > MAX_BODY_LENGTH) {
      fields.body = `must be at most ${MAX_BODY_LENGTH} characters`;
    }
    if (Object.keys(fields).length > 0) {
      throw new NoteValidationError(fields);
    }
    const note: Note = {
      id: randomUUID(),
      title,
      body,
      created_at: new Date().toISOString(),
    };
    await this.repository.save(note);
    this.publisher.noteCreated(note);
    return note;
  }

  async get(id: string): Promise<Note> {
    const note = await this.repository.get(id);
    if (note === undefined) {
      throw new NoteNotFoundError(id);
    }
    return note;
  }

  async list(offset: number, limit: number): Promise<NotePage> {
    const fields: Record<string, string> = {};
    if (!Number.isInteger(limit) || limit < 1 || limit > MAX_PAGE_SIZE) {
      fields.limit = `must be between 1 and ${MAX_PAGE_SIZE}`;
    }
    if (!Number.isInteger(offset) || offset < 0) {
      fields.offset = "must not be negative";
    }
    if (Object.keys(fields).length > 0) {
      throw new NoteValidationError(fields);
    }
    const { items, total } = await this.repository.list(offset, limit);
    return { items, total, offset, limit };
  }
}
