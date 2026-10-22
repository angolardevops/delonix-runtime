# The image compose.yaml and `docker build` use. It mirrors the two-stage
# Delonixfile (which `delonix build` uses and may extend with Delonix-only
# directives): keep the two in step when you change either.
#
#   docker build -t __NAME__:dev .          # or: docker compose build
#
# Stage 1 installs every dependency and runs `next build`; only the result —
# .next/standalone (server.js, the traced production node_modules, and the
# browser assets `pnpm build` copies into it) — reaches the runtime image. No
# package manager, no build cache, no source tree, no tests. .dockerignore
# keeps node_modules, .next, .env and .git out of the context.
FROM node:24-slim AS build
ENV NEXT_TELEMETRY_DISABLED=1 COREPACK_ENABLE_DOWNLOAD_PROMPT=0
WORKDIR /app
RUN corepack enable
COPY . .
# From the lock when the context has one (reproducible); a fresh project that
# has not run `pnpm install` yet resolves here instead, and says so.
RUN if [ -f pnpm-lock.yaml ]; then pnpm install --frozen-lockfile; \
    else echo "pnpm-lock.yaml not found: resolving now; run 'pnpm install' and commit it for reproducible images" && pnpm install --no-frozen-lockfile; fi
RUN pnpm build

FROM node:24-slim
# NEXT_MANUAL_SIG_HANDLE: the service's own ordered shutdown handles SIGTERM
# (docs/adr/0003-shutdown.md). HOSTNAME: server.js binds to it, and a container
# runtime otherwise sets it to the container's own name.
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 NEXT_MANUAL_SIG_HANDLE=true HOSTNAME=0.0.0.0 PORT=__PORT__
WORKDIR /app
COPY --from=build /app/.next/standalone ./
# A thin launcher that keeps `node` as PID 1 (so SIGTERM reaches the ordered
# shutdown). Pre-start work goes inside it, above its exec line.
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
ARG VERSION=dev
ENV APP_VERSION=${VERSION}
EXPOSE __PORT__
# The runtime probes liveness; readiness is for the load balancer.
HEALTHCHECK CMD node -e "fetch('http://127.0.0.1:__PORT__/api/v1/health/live').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"
STOPSIGNAL SIGTERM
# node:24-slim ships this unprivileged user (uid 1000); it owns nothing in the
# image and writes only to the manifest's tmpfs.
USER node
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["node", "server.js"]
