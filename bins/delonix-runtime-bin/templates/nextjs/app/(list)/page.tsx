import Link from "next/link";
import { after } from "next/server";
import { PROJECT_NAME } from "@/lib/project";
import { DEFAULT_PAGE_SIZE } from "@/lib/notes/notes";
import { getRuntime } from "@/lib/server/runtime";
import { NoteForm } from "./note-form";

// Rendered per request: the list comes from the running process.
export const dynamic = "force-dynamic";

// A Server Component: it calls the use case directly, on the server, instead
// of fetching its own API over HTTP. The browser only talks to the Route
// Handlers (the form below posts to /api/v1/notes).
export default async function HomePage() {
  const runtime = getRuntime();
  // The shutdown waits for a render in flight, like it does for an API request.
  after(runtime.lifecycle.begin());
  const page = await runtime.notes.list(0, DEFAULT_PAGE_SIZE);
  return (
    <>
      <h1>{PROJECT_NAME}</h1>
      <NoteForm />
      <h2 id="notes-heading">
        Notes ({page.items.length} of {page.total})
      </h2>
      {page.items.length === 0 ? (
        <p>No notes yet.</p>
      ) : (
        <ul className="notes" aria-labelledby="notes-heading">
          {page.items.map((note) => (
            <li key={note.id}>
              <Link href={`/notes/${note.id}`}>{note.title}</Link>
              <br />
              <time dateTime={note.created_at}>{note.created_at}</time>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
