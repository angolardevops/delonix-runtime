import type { FastifyInstance } from "fastify";
import {
  DEFAULT_PAGE_SIZE,
  MAX_BODY_LENGTH,
  MAX_PAGE_SIZE,
  MAX_TITLE_LENGTH,
  type NotesUseCases,
} from "./notes.js";

const noteSchema = {
  type: "object",
  required: ["id", "title", "body", "created_at"],
  properties: {
    id: { type: "string" },
    title: { type: "string" },
    body: { type: "string" },
    created_at: { type: "string", format: "date-time" },
  },
} as const;

export const noteInputSchema = {
  type: "object",
  required: ["title"],
  additionalProperties: false,
  properties: {
    title: { type: "string", maxLength: 10 * MAX_TITLE_LENGTH },
    body: { type: "string", maxLength: 10 * MAX_BODY_LENGTH },
  },
} as const;

const error = { $ref: "Error#" } as const;

/**
 * HTTP transport of the notes capability. The schemas check shape and types;
 * the rules (trimmed title, lengths, page bounds) live in the use cases, which
 * answer with every failing field at once.
 */
export async function notesRoutes(app: FastifyInstance, opts: { notes: NotesUseCases }) {
  app.post<{ Body: { title: string; body?: string } }>(
    "/",
    {
      schema: {
        summary: "Create a note.",
        body: noteInputSchema,
        response: { 201: noteSchema, 400: error, 413: error, 422: error },
      },
    },
    async (request, reply) => {
      const note = await opts.notes.create(request.body);
      return reply.code(201).header("location", `/api/v1/notes/${note.id}`).send(note);
    },
  );

  app.get<{ Querystring: { offset: number; limit: number } }>(
    "/",
    {
      schema: {
        summary: "List notes, newest first.",
        querystring: {
          type: "object",
          properties: {
            offset: { type: "integer", default: 0 },
            limit: {
              type: "integer",
              default: DEFAULT_PAGE_SIZE,
              description: `1–${MAX_PAGE_SIZE}`,
            },
          },
        },
        response: {
          200: {
            type: "object",
            required: ["items", "total", "offset", "limit"],
            properties: {
              items: { type: "array", items: noteSchema },
              total: { type: "integer" },
              offset: { type: "integer" },
              limit: { type: "integer" },
            },
          },
          422: error,
        },
      },
    },
    async (request) => opts.notes.list(request.query.offset, request.query.limit),
  );

  app.get<{ Params: { id: string } }>(
    "/:id",
    {
      schema: {
        summary: "Get one note.",
        params: { type: "object", required: ["id"], properties: { id: { type: "string" } } },
        response: { 200: noteSchema, 404: error },
      },
    },
    async (request) => opts.notes.get(request.params.id),
  );
}
