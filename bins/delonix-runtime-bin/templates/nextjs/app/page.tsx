export default function Home() {
  return (
    <main style={{ font: "16px system-ui", margin: "4rem auto", maxWidth: "40rem" }}>
      <h1>__NAME__</h1>
      <p>
        Scaffolded by <code>delonix init --template nextjs</code>. Health:{" "}
        <a href="/api/v1/health/live">/api/v1/health/live</a>
      </p>
    </main>
  );
}
