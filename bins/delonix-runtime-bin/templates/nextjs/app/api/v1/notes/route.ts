import { DEFAULT_PAGE_SIZE } from "@/lib/notes/notes";
import {
  decodeNoteInput,
  HttpError,
  intParam,
  json,
  parseJsonObject,
  readBody,
  route,
} from "@/lib/http/route";

export const dynamic = "force-dynamic";

export const POST = route(async (request, _context, runtime) => {
  const raw = await readBody(request, runtime.config.maxBodyBytes);
  const input = decodeNoteInput(parseJsonObject(raw));
  const note = await runtime.notes.create(input);
  return json(201, note, { location: `/api/v1/notes/${note.id}` });
});

export const GET = route(async (request, _context, runtime) => {
  const url = new URL(request.url);
  const fields: Record<string, string> = {};
  const offset = intParam(url, "offset", 0, fields);
  const limit = intParam(url, "limit", DEFAULT_PAGE_SIZE, fields);
  if (Object.keys(fields).length > 0) {
    throw new HttpError(422, "validation_failed", "invalid input", fields);
  }
  return json(200, await runtime.notes.list(offset, limit));
});
