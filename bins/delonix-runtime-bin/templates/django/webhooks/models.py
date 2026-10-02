from django.db import models


class InboundDelivery(models.Model):
    """The id of every inbound delivery accepted in the last minutes. The
    unique key is what makes a sender's retry harmless: across restarts and
    across replicas that share the database."""

    webhook_id = models.CharField(max_length=255, unique=True)
    received_at = models.DateTimeField(db_index=True)

    def __str__(self) -> str:
        return self.webhook_id
