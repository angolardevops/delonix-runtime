from django.apps import AppConfig


class WebhooksConfig(AppConfig):
    name = "webhooks"
    default_auto_field = "django.db.models.BigAutoField"

    def ready(self) -> None:
        from webhooks import receivers

        receivers.connect()
