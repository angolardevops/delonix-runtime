# 0001 — Plain Django views, an app per domain, rules in a service module

Status: accepted (template default)

## Context
The service needs routing, JSON, persistence, validation and one error
shape. Django provides routing, the ORM and migrations; Django REST Framework
would add serializers, viewsets, content negotiation and its own exception
handling.

## Decision
- Function views behind `core.http.api_view` (method dispatch, 405, the
  error shape, strict JSON parsing). No DRF.
- One app per domain (`notes`), with `services.py` holding the use cases and
  rules. Views decode and encode; models persist. The ORM is used directly
  in the services — no repository layer.
- The capability announces events with Django signals sent on commit;
  adapters (`webhooks`) subscribe.
- Dependency direction is a test (`tests/test_architecture.py`), not a
  convention.

## Alternatives
- DRF: worth it for many resources, browsable docs, pluggable
  auth/permissions/throttling. For three endpoints it is a second validation
  and error system to keep consistent with the contract.
- Fat models / validation in `Model.clean`: couples rules to persistence and
  runs only when someone calls `full_clean`.
- A repository interface over the ORM: an abstraction with one
  implementation; tests already run against a real (in-memory) database.

## Trade-off
Validation is written by hand in the service (about twenty lines). When the
number of resources grows, DRF or `pydantic` schemas become cheaper than
hand-written checks; the service functions stay the same.
