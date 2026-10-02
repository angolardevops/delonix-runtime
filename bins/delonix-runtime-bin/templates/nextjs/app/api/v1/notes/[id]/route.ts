import { json, route } from "@/lib/http/route";

export const dynamic = "force-dynamic";

// `params` is a Promise since Next.js 15.
export const GET = route<{ params: Promise<{ id: string }> }>(async (_request, context, runtime) => {
  const { id } = await context.params;
  return json(200, await runtime.notes.get(id));
});
