<?php

namespace App\Logging;

use Illuminate\Log\Logger;
use Monolog\Handler\FormattableHandlerInterface;
use Monolog\LogRecord;
use OpenTelemetry\API\Trace\Span;

/**
 * `tap` of the `json` log channel (config/logging.php): installs the JSON
 * formatter and a processor that adds the current trace and span ids, so a
 * log line and the trace of the same request find each other.
 */
final class ConfigureJsonLogging
{
    public function __invoke(Logger $logger): void
    {
        $monolog = $logger->getLogger();
        if (! $monolog instanceof \Monolog\Logger) {
            return;
        }
        $formatter = new JsonLogFormatter([
            'service' => (string) config('service.name'),
            'version' => (string) config('service.version'),
            'environment' => (string) config('app.env'),
        ]);
        foreach ($monolog->getHandlers() as $handler) {
            if ($handler instanceof FormattableHandlerInterface) {
                $handler->setFormatter($formatter);
            }
        }
        $monolog->pushProcessor(static function (LogRecord $record): LogRecord {
            $context = Span::getCurrent()->getContext();
            if (! $context->isValid()) {
                return $record;
            }

            return $record->with(extra: $record->extra + [
                'trace_id' => $context->getTraceId(),
                'span_id' => $context->getSpanId(),
            ]);
        });
    }
}
