<?php

namespace App\Http\Errors;

use App\Notes\InvalidNote;
use App\Notes\NoteNotFound;
use App\Webhooks\InvalidWebhook;
use Illuminate\Database\Eloquent\ModelNotFoundException;
use Illuminate\Http\Exceptions\PostTooLargeException;
use Illuminate\Http\JsonResponse;
use Illuminate\Http\Request;
use Illuminate\Validation\ValidationException;
use Symfony\Component\HttpKernel\Exception\HttpExceptionInterface;
use Symfony\Component\HttpKernel\Exception\MethodNotAllowedHttpException;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;
use Throwable;

/**
 * The one error shape of the API:
 *   {"error": {"code", "message", "fields"?, "request_id"}}
 * Codes: validation_failed 422, malformed_json 400, body_too_large 413,
 * not_found 404, internal 500 (message only, never a stack trace — the
 * exception itself is logged by Laravel's handler).
 */
final class ErrorRenderer
{
    public static function render(Throwable $e, Request $request): JsonResponse
    {
        [$status, $code, $message, $fields] = self::classify($e);

        $error = ['code' => $code, 'message' => $message];
        if ($fields !== null) {
            $error['fields'] = $fields;
        }
        $error['request_id'] = (string) $request->attributes->get('request_id', '');

        return new JsonResponse(['error' => $error], $status);
    }

    /** @return array{0: int, 1: string, 2: string, 3: array<string, string>|null} */
    public static function classify(Throwable $e): array
    {
        return match (true) {
            $e instanceof ApiError => [$e->status, $e->errorCode, $e->getMessage(), $e->fields],
            $e instanceof ValidationException => [422, 'validation_failed', 'invalid input', self::firstMessages($e->errors())],
            $e instanceof InvalidNote => [422, 'validation_failed', 'invalid input', $e->fields],
            $e instanceof InvalidWebhook => [$e->status, $e->errorCode, $e->getMessage(), null],
            $e instanceof NoteNotFound => [404, 'not_found', 'note not found', null],
            $e instanceof ModelNotFoundException, $e instanceof NotFoundHttpException => [404, 'not_found', 'not found', null],
            $e instanceof PostTooLargeException => [413, 'body_too_large', 'request body too large', null],
            $e instanceof MethodNotAllowedHttpException => [405, 'method_not_allowed', 'method not allowed', null],
            $e instanceof HttpExceptionInterface && $e->getStatusCode() < 500 => [$e->getStatusCode(), 'bad_request', 'request refused', null],
            default => [500, 'internal', 'internal error', null],
        };
    }

    /**
     * @param  array<string, array<int, string>>  $errors
     * @return array<string, string>
     */
    private static function firstMessages(array $errors): array
    {
        $fields = [];
        foreach ($errors as $field => $messages) {
            $fields[$field] = (string) ($messages[0] ?? 'is invalid');
        }

        return $fields;
    }
}
