import Link from "next/link";
import { notFound } from "next/navigation";
import { after } from "next/server";
import { isNoteNotFound } from "@/lib/notes/notes";
import { getRuntime } from "@/lib/server/runtime";

export const dynamic = "force-dynamic";

export default async function NotePage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const runtime = getRuntime();
  after(runtime.lifecycle.begin());
  const note = await runtime.notes.get(id).catch((err: unknown) => {
    // Renders app/not-found.tsx with a 404 status.
    if (isNoteNotFound(err)) notFound();
    throw err;
  });
  return (
    <article>
      <h1>{note.title}</h1>
      <p>
        <time dateTime={note.created_at}>{note.created_at}</time>
      </p>
      <p style={{ whiteSpace: "pre-wrap" }}>{note.body}</p>
      <p>
        <Link href="/">Back to the notes</Link>
      </p>
    </article>
  );
}
