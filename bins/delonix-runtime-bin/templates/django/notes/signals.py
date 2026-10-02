"""Events this capability announces. Other apps subscribe (the webhooks app
turns `note_created` into an outbound webhook); notes never imports them."""

from django.dispatch import Signal

# Sent once the transaction that stored the note has committed.
# Keyword argument: note (notes.models.Note).
note_created = Signal()
