# __NAME__ (Django)

A Django project scaffolded by `delonix init --template django` — **uv**-managed,
12-factor settings, health probes, gunicorn, and a Delonix manifest. Minimal (no
DB) so it runs out of the box; add `DATABASES` + `django.contrib.*` when you add models.

## Run it locally (uv)

```bash
uv sync
uv run python manage.py runserver __PORT__
curl localhost:__PORT__/api/v1/health/live
```

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```
