"""The example capability: create, read and list short notes.

Layers inside the app, outermost first:
    urls.py / views.py   HTTP transport — decode, call a service, encode
    services.py          use cases and rules (validation, paging, events)
    models.py            persistence (the Django ORM)
    signals.py           the event this capability announces (note_created)
Views never touch the ORM; services never touch HTTP (tests/test_architecture.py).
"""
