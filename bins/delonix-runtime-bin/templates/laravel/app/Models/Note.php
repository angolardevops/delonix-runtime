<?php

namespace App\Models;

use Carbon\CarbonImmutable;
use Illuminate\Database\Eloquent\Model;

/**
 * Eloquent row of the `notes` table. Only App\Persistence uses it; the
 * capability works with App\Notes\Note.
 *
 * @property string $id
 * @property string $title
 * @property string $body
 * @property CarbonImmutable $created_at
 */
class Note extends Model
{
    public $incrementing = false;

    public $timestamps = false;

    /** Microseconds are stored: ordering stays stable within one second. */
    protected $dateFormat = 'Y-m-d H:i:s.u';

    protected $keyType = 'string';

    protected $fillable = ['id', 'title', 'body', 'created_at'];

    protected function casts(): array
    {
        return ['created_at' => 'immutable_datetime'];
    }
}
