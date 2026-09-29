# ADR-0060: Cache a registry token on disk only when it was issued anonymously

- **Status:** Proposed
- **Date:** 2026-09-29
- **Deciders:** Walter Angolar
- **Relates to:** the image performance series (#518–#538, #605; its section in AGENTS.md
  lists this as item P3, declined until an ADR decides it), ADR-0010 (the engine keeps no
  remote surface), the credential store (`delonix-oci::auth`, `delonix image login`)

## Context

Every registry command that talks to a registry starts from nothing. The `Client` in
`delonix-oci::registry` holds its bearer token in memory, so within one pull every layer worker
shares the token the manifest request obtained. The next command, a second later, pays the
whole handshake again:

1. a request without a token, answered `401` with a `WWW-Authenticate` challenge;
2. a request to the token service named in the challenge;
3. the request it wanted to make in the first place.

**Measured** (2026-09-29, this host, Docker Hub, `curl`, 5 runs, `library/alpine:3.20`, a
manifest request — which is what a warm pull of an image already in the store makes):

| call | time |
|---|---|
| the `401` | 0.62–0.93 s |
| the token request (`auth.docker.io`) | 0.33–0.70 s |
| the manifest, with the token | 0.72–0.86 s |

With a token already in hand only the last row remains: about 1 s saved per command, more than
half of a warm pull. That is an upper bound. Each `curl` opens its own TLS connection, while
the engine reuses its connection to the registry between the `401` and the manifest; the
token service is a different host and always costs a connection.

**What a token is, measured on the one Docker Hub issues without credentials** (JWT claims):
`sub` is empty, `access` is `pull` on the one repository asked for (`library/node`),
`aud` is the registry, and it expires 300 s after issue. It identifies nobody and grants
nothing that anyone on the internet cannot obtain with the same request. A token issued with
credentials is different: it can grant `pull` (or `push`) on private repositories, and it
carries the account.

**What others do.** The Docker CLI does not keep bearer tokens on disk. containerd keeps them in
memory, which works because containerd is a daemon. This engine is daemonless (guardrail 1), so
a cache that outlives one command can only live on disk.

**What the cache cannot do.** A token is scoped to one repository and one action set, and lives
300 s on Docker Hub. The saving applies only to a second command on the SAME repository within
that window: a CI loop that pulls the same public base image in several jobs, a `compose up`
re-run, a retry after a failure, a `kind` node image checked twice.

## Decision

**D1 — Cache only tokens obtained without credentials.** When the client has no credentials
for the registry (no `delonix image login` for that host), a token it obtains is written to
`<root>/auth/tokens/<host>/<sha256(scope)>.json`, mode `0600`, with the atomic write the
credential store already uses (`write_atomic_mode`). When credentials exist for the host,
nothing is read from the cache and nothing is written to it. A token that could read a private
repository never reaches the disk.

**D2 — Only the `pull` scope.** A push asks for `pull,push` and, in practice, always goes with
credentials; D1 already excludes it. The rule is written down anyway, so that a registry configured
to issue push tokens without credentials does not get them cached.

**D3 — The expiry comes from the token, and is shortened.** The entry is valid until the earlier
of the JWT `exp` claim (when the token is a JWT) and `issued_at + expires_in` from the token
response, minus 30 s. A token that cannot be read either way is not cached. An entry past that
time is never used, and is removed when the next token for the host is written.

**D4 — The cache can only save a request, never fail one.**
- A missing, unreadable or corrupt entry is a cache miss: the client takes the ordinary path.
- A request made with a cached token that gets a `401` drops the entry and takes the ordinary
  path, once. It never retries with the same token.
- A failure to write the cache is logged at `debug` and the command continues.

**D5 — Identity of an entry.** The key is the registry host plus the exact scope string asked
for. Two hosts never share an entry, and neither do two repositories on one host.

## Alternatives considered

- **Do nothing.** This keeps the current cost: about 1 s per command against a public
  repository. It is the safe default, and it was the recommendation until the anonymous token
  was measured. It is still reasonable if the engine should keep nothing about registries
  between commands on principle.
- **Cache every token, credentialed ones included.** This saves the same second on private
  images too, but it puts a credential at rest that can read private repositories for up to its
  lifetime, readable by anything running as the user. The Docker CLI does not do this.
  Rejected.
- **Cache in memory only.** This gains nothing across commands, and across commands is the
  only place the cost is paid.
- **A token broker process.** This would share tokens the way containerd does, but it is a
  daemon (guardrail 1), for a saving of one second.

## Consequences

- **Easier:** a warm pull of a public image skips two of its three network calls when the same
  repository was used in the last ~4.5 minutes.
- **Harder:**
  - a new directory under the state root to keep bounded (D3 prunes expired entries on write);
  - one more thing a `system snapshot` must leave out (a token is not state worth restoring).
- **Security:** an anonymous token on disk grants what the same request to the token service
  grants anyone. The mode is `0600` anyway, like everything else under the state root.
- **Rate limits:** unaffected, per Docker Hub's documentation, which counts manifest requests
  and not token requests (not measured here). The anonymous token carries `pull_limit: 100`
  per `21600` s.

## Validation required before merge of an implementation

1. A test that a client WITH credentials neither reads nor writes the cache. It must fail if the
   check is removed.
2. A test that a `401` on a cached token drops the entry and succeeds on the ordinary path.
3. A test that an expired or corrupt entry is a miss.
4. The measurement repeated with the implementation: a warm `image pull alpine:3.20` twice in a
   row, isolated root, comparing the second pull with and without the cache.
