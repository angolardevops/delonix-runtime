# The image compose.yaml and `docker build` use. It is the same two-stage pnpm
# build as the Delonixfile (which `delonix build` uses and may extend with
# Delonix-only directives): keep the two in step when you change either.
#
#   docker build -t __NAME__:dev .          # or: docker compose build
#
# Stage 1 installs every dependency, compiles TypeScript and prunes to the
# production dependencies; only package.json, node_modules (prod) and dist
# reach the runtime image — no source tree, no dev tooling, no tests.
# .dockerignore keeps .env and node_modules out of the context.
FROM node:24-slim AS build
WORKDIR /app
RUN corepack enable
COPY . .
# From the lock when the context has one (reproducible); a fresh project that
# has not run `pnpm install` yet resolves here instead, and says so.
RUN if [ -f pnpm-lock.yaml ]; then pnpm install --frozen-lockfile; \
    else echo "pnpm-lock.yaml not found: resolving now; run 'pnpm install' and commit it for reproducible images" && pnpm install; fi
RUN pnpm build && pnpm prune --prod

FROM node:24-slim
ENV NODE_ENV=production
WORKDIR /app
COPY --from=build /app/package.json ./package.json
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
COPY --from=build /app/docker/entrypoint.sh ./docker/entrypoint.sh
RUN chmod +x /app/docker/entrypoint.sh
ARG VERSION=dev
ENV SERVICE_VERSION=${VERSION}
EXPOSE __PORT__
# The runtime probes liveness; readiness is for the load balancer.
HEALTHCHECK CMD node -e "fetch('http://127.0.0.1:__PORT__/api/v1/health/live').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"
STOPSIGNAL SIGTERM
# node:24-slim ships this unprivileged user (uid 1000); it owns nothing in the
# image and writes only to the manifest's tmpfs.
USER node
# The entrypoint is a thin shell wrapper so a future release step (a migration,
# a warm-up) has one place to live; today it just execs the command.
ENTRYPOINT ["/app/docker/entrypoint.sh"]
CMD ["node", "--import", "./dist/instrumentation.js", "dist/index.js"]
