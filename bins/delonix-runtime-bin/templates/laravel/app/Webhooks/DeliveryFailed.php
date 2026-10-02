<?php

namespace App\Webhooks;

use RuntimeException;

/** The receiver answered, but not with a 2xx. */
final class DeliveryFailed extends RuntimeException
{
    public function __construct(public readonly int $status)
    {
        parent::__construct("webhook receiver answered {$status}");
    }

    /** 429 and 5xx are worth another attempt; any other status is final. */
    public function isRetryable(): bool
    {
        return $this->status === 429 || $this->status >= 500;
    }
}
