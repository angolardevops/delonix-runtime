import Link from "next/link";

export default function NotFound() {
  return (
    <>
      <h1>Not found</h1>
      <p>There is nothing at this address.</p>
      <p>
        <Link href="/">Back to the notes</Link>
      </p>
    </>
  );
}
