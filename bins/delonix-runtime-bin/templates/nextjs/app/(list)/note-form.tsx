"use client";

import { useRouter } from "next/navigation";
import { useState, type FormEvent } from "react";

interface ApiError {
  error: { code: string; message: string; fields?: Record<string, string>; request_id?: string };
}

// The only interactive part of the page, so the only Client Component. It
// posts to the Route Handler and asks the server to render the list again;
// it holds no secret and imports nothing from lib/server.
export function NoteForm() {
  const router = useRouter();
  const [pending, setPending] = useState(false);
  const [fields, setFields] = useState<Record<string, string>>({});
  const [failure, setFailure] = useState("");

  async function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const form = event.currentTarget;
    const data = new FormData(form);
    setPending(true);
    setFields({});
    setFailure("");
    try {
      const response = await fetch("/api/v1/notes", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          title: String(data.get("title") ?? ""),
          body: String(data.get("body") ?? ""),
        }),
      });
      if (response.ok) {
        form.reset();
        router.refresh();
        return;
      }
      const problem = (await response.json()) as ApiError;
      if (problem.error.fields) setFields(problem.error.fields);
      else setFailure(`${problem.error.message} (request ${problem.error.request_id ?? "unknown"})`);
    } catch {
      setFailure("The note could not be sent. Check the connection and try again.");
    } finally {
      setPending(false);
    }
  }

  return (
    <form onSubmit={onSubmit} aria-labelledby="new-note-heading" noValidate>
      <h2 id="new-note-heading">New note</h2>
      <label htmlFor="note-title">Title</label>
      <input
        id="note-title"
        name="title"
        type="text"
        required
        maxLength={200}
        aria-invalid={fields.title ? true : undefined}
        aria-describedby={fields.title ? "note-title-error" : undefined}
      />
      {fields.title && (
        <p id="note-title-error" className="error">
          Title {fields.title}
        </p>
      )}
      <label htmlFor="note-body">Body (optional)</label>
      <textarea
        id="note-body"
        name="body"
        rows={4}
        maxLength={10000}
        aria-invalid={fields.body ? true : undefined}
        aria-describedby={fields.body ? "note-body-error" : undefined}
      />
      {fields.body && (
        <p id="note-body-error" className="error">
          Body {fields.body}
        </p>
      )}
      <button type="submit" disabled={pending}>
        {pending ? "Saving…" : "Add note"}
      </button>
      <p role="alert" className="error">
        {failure}
      </p>
    </form>
  );
}
