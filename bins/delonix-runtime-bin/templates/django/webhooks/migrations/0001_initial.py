from django.db import migrations, models


class Migration(migrations.Migration):
    initial = True

    dependencies: list[tuple[str, str]] = []

    operations = [
        migrations.CreateModel(
            name="InboundDelivery",
            fields=[
                ("id", models.BigAutoField(auto_created=True, primary_key=True, serialize=False, verbose_name="ID")),
                ("webhook_id", models.CharField(max_length=255, unique=True)),
                ("received_at", models.DateTimeField(db_index=True)),
            ],
        ),
    ]
