<?php

namespace App\Http\Controllers\Api\V1;

use App\Http\Errors\ApiError;
use App\Webhooks\InboundProcessor;
use App\Webhooks\StandardWebhook;
use Illuminate\Http\Request;
use Illuminate\Http\Response;

/**
 * POST /api/v1/webhooks/inbound — Standard Webhooks, verified over the raw
 * request bytes ($request->getContent()), before any JSON parsing. Answers
 * 204 for an accepted delivery and for a duplicate of one.
 */
final class InboundWebhookController
{
    public function __invoke(Request $request, InboundProcessor $processor): Response
    {
        $secret = (string) config('service.webhooks.inbound_secret');
        if ($secret === '') {
            throw ApiError::notFound(); // endpoint off: indistinguishable from no route
        }
        $raw = $request->getContent();
        $id = $request->headers->get(StandardWebhook::HEADER_ID);
        StandardWebhook::verify(
            StandardWebhook::parseSecret($secret),
            $id,
            $request->headers->get(StandardWebhook::HEADER_TIMESTAMP),
            $request->headers->get(StandardWebhook::HEADER_SIGNATURE),
            $raw,
            time(),
        );
        $processor->process((string) $id, $raw);

        return response()->noContent();
    }
}
