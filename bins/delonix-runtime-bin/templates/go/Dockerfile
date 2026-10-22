# The image compose.yaml and `docker build` use. It is the same two-stage Go
# build as the Delonixfile (which `delonix build` uses and may extend with
# Delonix-only directives): keep the two in step when you change either.
#
#   docker build -t __NAME__:dev .          # or: docker compose build
#
# Stage 1 compiles a static binary; only that binary reaches the runtime image
# (a few MB instead of the ~300 MB toolchain). go.mod and go.sum are copied
# first so the module download is cached until they change. .dockerignore keeps
# .env and the local build out of the context.
FROM golang:__TEMPLATE_VERSION__-alpine AS build
ENV CGO_ENABLED=0 GOFLAGS=-trimpath
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY cmd ./cmd
COPY internal ./internal
ARG VERSION=dev
RUN go build -ldflags "-s -w -X main.version=${VERSION}" -o /out/server ./cmd/__NAME__

FROM alpine:3.24
# CA roots for the outbound webhook over https, and an unprivileged user for
# the service (uid 10001). Nothing in the image belongs to that user: it writes
# nowhere but the manifest's tmpfs.
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 10001 app && adduser -S -D -H -u 10001 -G app app
COPY --from=build /out/server /usr/local/bin/server
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
EXPOSE __PORT__
# The runtime probes liveness; readiness is for the load balancer.
HEALTHCHECK CMD wget -qO- http://127.0.0.1:__PORT__/api/v1/health/live || exit 1
STOPSIGNAL SIGTERM
USER app
# The entrypoint is the one place for pre-flight; it execs the server, which
# owns readiness, drain and the exit code.
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/usr/local/bin/server"]
