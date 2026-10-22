"""`manage.py consume_events`: run the RabbitMQ consumer in the foreground.

A separate process from the web server (run it as its own container/replica):
    python manage.py consume_events
With RABBITMQ_URL unset it prints that there is nothing to consume and exits 0,
so the command is safe to wire unconditionally. SIGINT/SIGTERM stop it cleanly.
"""

from __future__ import annotations

import signal
from typing import Any

from django.conf import settings
from django.core.management.base import BaseCommand

from messaging.consumer import Consumer


class Command(BaseCommand):
    help = "Consume note.created events from RabbitMQ (RABBITMQ_URL)."

    def handle(self, *args: Any, **options: Any) -> None:
        url = settings.APP.rabbitmq_url
        if not url:
            self.stdout.write("RABBITMQ_URL is not set; nothing to consume.")
            return

        consumer = Consumer(url)

        # start_consuming() blocks; SIGTERM/SIGINT raise KeyboardInterrupt out
        # of pika's I/O loop so the finally in run() closes the connection.
        def stop(signum: int, frame: Any) -> None:
            raise KeyboardInterrupt

        signal.signal(signal.SIGTERM, stop)
        try:
            consumer.run()
        except KeyboardInterrupt:
            self.stdout.write("consumer stopped.")
