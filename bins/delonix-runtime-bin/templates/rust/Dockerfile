# The image compose.yaml and `docker build` use. It is the same two-stage cargo
# build as the Delonixfile (which `delonix build` uses and may extend with
# Delonix-only directives): keep the two in step when you change either.
#
#   docker build -t __NAME__:dev .          # or: docker compose build
#
# Stage 1 compiles a release binary against glibc; only the binary, the CA roots
# and a tiny entrypoint reach the runtime image — no toolchain, no source tree,
# no build cache. Manifests are copied first so the dependency build is cached
# until they change. .dockerignore keeps .env, target/ and the git tree out of
# the context.
FROM rust:__TEMPLATE_VERSION__-slim AS build
WORKDIR /src
# Cache dependencies: a manifest-only layer that only rebuilds when they change.
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release --quiet || true
COPY . /src
RUN cargo build --release --locked --quiet 2>/dev/null || cargo build --release --quiet
RUN cp "target/release/__NAME__" /usr/local/bin/__NAME__

FROM debian:bookworm-slim
# CA roots for the outbound webhook over https, and curl for the healthcheck.
# An unprivileged user for the service (uid 10001): it owns nothing in the image
# and writes only to the manifest's tmpfs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 app \
    && useradd --system --uid 10001 --gid app --no-create-home --home-dir /nonexistent app
COPY --from=build /usr/local/bin/__NAME__ /usr/local/bin/__NAME__
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
ENV APP_ENV=production LOG_LEVEL=info
EXPOSE __PORT__
# The runtime probes liveness; readiness is for the load balancer.
HEALTHCHECK CMD curl -fsS http://127.0.0.1:__PORT__/api/v1/health/live || exit 1
STOPSIGNAL SIGTERM
USER app
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
