# 0003 — FrankenPHP in classic mode, configuration cached at container start

Status: accepted (template default)

## Context
A PHP service needs a web server in front of PHP. The image must carry no
secret, and Laravel's `config:cache` freezes whatever environment it sees.

## Decision
- Two stages: `composer:2` installs dependencies (from `composer.lock` when
  the context has one), `dunglas/frankenphp:1-php8.4` runs the application —
  one process serving HTTP and PHP, configured by the `Caddyfile`.
- Classic mode (one PHP execution per request), not worker mode.
- No `.env`, no `APP_KEY`, no `config:cache` at build. The entrypoint
  validates the configuration (`app:check-config`, exit 1 on any problem),
  caches it into `/tmp`, migrates, then starts the server. The same image
  runs the queue worker (`command: ["worker"]`).
- uid 0 inside the container, read-only root filesystem, tmpfs for `/tmp`,
  `/data`, `/config`, a volume for the SQLite file.

## Alternatives
- **nginx + php-fpm**: two processes and a supervisor, or two containers.
- **Worker mode (Laravel Octane)**: several times the throughput, at the
  price of state that survives between requests; adopt it deliberately, with
  `laravel/octane`, not by default.
- **`config:cache` at build**: bakes the build machine's environment into a
  layer, so runtime variables — a secret `APP_KEY` among them — are ignored.
- **`USER` non-root**: better defence in depth where the host has a
  subordinate uid range; fails to start where it has none.
- **`install-php-extensions pcntl`**: lets the queue worker finish the job in
  hand on SIGTERM and enforce a job's `$timeout`. It compiles from the PHP
  source tarball, and unpacking that fails in a rootless `delonix build`
  (measured: `tar: Cannot change mode … Operation not permitted`). Left out so
  the image builds everywhere; add the line back when building as root, or use
  a base image that ships the extension.

## Trade-off
Each request boots the framework (a few milliseconds with opcache), and
start-up runs three artisan commands before listening. Rootless Delonix maps
uid 0 to the unprivileged host user, which bounds a compromise on the host
but is not the same as a non-root process inside the container. Without ext-pcntl a SIGTERM
stops the worker at once: a delivery in hand is retried after `retry_after`
(90 s), which the webhook's at-least-once contract already allows.
