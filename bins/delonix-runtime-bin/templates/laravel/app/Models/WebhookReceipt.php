<?php

namespace App\Models;

use Carbon\CarbonImmutable;
use Illuminate\Database\Eloquent\Model;

/**
 * One accepted inbound webhook. The unique key on `webhook_id` is the
 * de-duplication: a second delivery with the same id cannot be inserted.
 *
 * @property string $webhook_id
 * @property CarbonImmutable $received_at
 */
class WebhookReceipt extends Model
{
    public $incrementing = false;

    public $timestamps = false;

    /** Microseconds are stored: ordering stays stable within one second. */
    protected $dateFormat = 'Y-m-d H:i:s.u';

    protected $primaryKey = 'webhook_id';

    protected $keyType = 'string';

    protected $fillable = ['webhook_id', 'received_at'];

    protected function casts(): array
    {
        return ['received_at' => 'immutable_datetime'];
    }
}
