# AGENTS.md — working on __NAME__

Short on purpose. The README explains the service; this file tells an agent
(or a new contributor) how to change it safely.

## Map
- Capability and rules: `app/Notes/` (plain PHP: no Illuminate, no Eloquent, no telemetry).
- Transport: `app/Http/` (controllers, Form Requests, middleware, error shape).
- Adapters: `app/Persistence/` + `app/Models/` (Eloquent), `app/Webhooks/` + `app/Jobs/` (signing, queue).
- Wiring: `app/Providers/AppServiceProvider.php` — the only place that binds ports to adapters.
- Sources of truth: routes → `api/openapi.yaml`; dependency rules →
  `tests/Architecture/DependencyDirectionTest.php`; configuration →
  `config/service.php` + `app/Support/ConfigValidator.php` + `.env.example`;
  schema → `database/migrations/`; decisions → `docs/adr/`.

## Verify every change
```bash
composer check        # lock present, Pint, syntax lint, PHPStan, PHPUnit — must pass before a commit
composer audit-deps   # when dependencies change (network)
```
With the server running (`composer serve`), `composer smoke` must print `smoke: OK`.
PHP 8.4 is required (the lock is resolved for it); `composer format` fixes style.

## Rules
- A new route updates `api/openapi.yaml` in the same change (the contract test fails otherwise).
- A new dependency of `app/Notes` goes behind an interface declared in `app/Notes`.
- A schema change is a new migration file; never edit one that has shipped.
- New configuration: `config/service.php` (read with `config()`, never `env()` outside
  `config/`), `ConfigValidator`, `.env.example` and the README table.
- Never log request bodies, headers or query strings; sensitive keys are redacted by key name.
- Secrets only from the environment. Never commit `.env`, an `APP_KEY` or a real `whsec_` value.
- Commit `composer.lock`; change it only with `composer require` / `composer update`.

## Out of scope for a routine task
Changing the webhook format, the error shape, or the `/api/v1` contract in a
breaking way needs an ADR in `docs/adr/` first.
