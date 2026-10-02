// Shown while the list page streams. It lives in the (list) route group so it
// does not wrap /notes/[id]: a page behind a loading boundary starts
// streaming with status 200 before it can call notFound(), and that page
// should answer a real 404.
export default function Loading() {
  return (
    <p role="status" aria-live="polite">
      Loading…
    </p>
  );
}
