"use client";

// Error boundaries must be Client Components. In production Next.js replaces
// the message of a server error with a digest, so nothing internal is shown;
// the digest matches a server log line.
export default function ErrorPage({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  return (
    <div role="alert">
      <h1>Something went wrong</h1>
      <p>The page could not be rendered.{error.digest ? ` Reference: ${error.digest}` : ""}</p>
      <button type="button" onClick={() => reset()}>
        Try again
      </button>
    </div>
  );
}
