# The image compose.yaml and `docker build` use. It is the same two-stage node
# build as the Delonixfile (which `delonix build` uses and may extend with
# Delonix-only directives): keep the two in step when you change either. The one
# difference is the launcher — this image goes through docker/entrypoint.sh so a
# `docker run` can pass extra node flags; the Delonixfile CMDs the same command.
#
#   docker build -t __NAME__:dev .          # or: docker compose build
#
# Stage 1 installs every dependency, compiles with the Nest CLI and prunes to
# the production dependencies; only those and the compiled dist/ reach the
# runtime image — no TypeScript, CLI, test, lint tooling or package manager.
# .dockerignore keeps .env, node_modules and .git out of the context.
FROM node:24-slim AS build
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0 CI=true
WORKDIR /app
# pnpm at the version pinned in package.json ("packageManager").
RUN corepack enable
COPY . /app
# From the lock when the project has one (reproducible); a fresh project without
# pnpm-lock.yaml resolves now. Commit pnpm-lock.yaml: see the README.
RUN --mount=type=cache,target=/pnpm/store if [ -f pnpm-lock.yaml ]; then pnpm install --frozen-lockfile --store-dir /pnpm/store; else pnpm install --store-dir /pnpm/store; fi
# Drop the dev dependencies here, so the runtime stage needs no package manager.
RUN pnpm build && pnpm prune --prod

FROM node:24-slim
ENV NODE_ENV=production APP_ENV=production
WORKDIR /app
COPY --from=build /app/package.json ./package.json
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
COPY --from=build /app/docker/entrypoint.sh ./docker/entrypoint.sh
ARG VERSION=dev
ENV APP_VERSION=${VERSION}
EXPOSE __PORT__
# The runtime probes liveness; readiness is for the load balancer.
HEALTHCHECK CMD node -e "fetch('http://127.0.0.1:__PORT__/api/v1/health/live').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"
STOPSIGNAL SIGTERM
# node:24-slim ships this unprivileged user (uid 1000); it owns nothing in the
# image and writes only to the manifest's tmpfs.
USER node
# The entry point loads the instrumentation before the application and then
# execs node, so SIGTERM reaches the process directly (see docker/entrypoint.sh).
ENTRYPOINT ["sh", "/app/docker/entrypoint.sh"]
