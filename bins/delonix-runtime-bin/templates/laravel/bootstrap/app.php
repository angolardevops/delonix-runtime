<?php

use App\Http\Errors\ErrorRenderer;
use App\Http\Middleware\AssignRequestId;
use App\Http\Middleware\LimitRequestBody;
use App\Http\Middleware\LogRequest;
use App\Http\Middleware\RejectMalformedJson;
use App\Http\Middleware\TraceRequest;
use App\Notes\InvalidNote;
use App\Notes\NoteNotFound;
use Illuminate\Foundation\Application;
use Illuminate\Foundation\Configuration\Exceptions;
use Illuminate\Foundation\Configuration\Middleware;
use Illuminate\Http\Request;

// The service is an HTTP API only: no web routes, no sessions, no cookies.
return Application::configure(basePath: dirname(__DIR__))
    ->withRouting(
        api: __DIR__.'/../routes/api.php',
        commands: __DIR__.'/../routes/console.php',
    )
    ->withMiddleware(function (Middleware $middleware): void {
        // Outermost first: the request id exists before anything logs, the
        // server span wraps everything below it (routing included), and the
        // access log line is written inside the span so it carries trace_id.
        $middleware->prepend([AssignRequestId::class, TraceRequest::class, LogRequest::class]);
        $middleware->alias([
            'body.limit' => LimitRequestBody::class,
            'json.object' => RejectMalformedJson::class,
        ]);
    })
    ->withExceptions(function (Exceptions $exceptions): void {
        // Every error leaves in ONE shape: {"error":{code,message,fields?,request_id}}.
        // Stack traces go to the log, never to the client (APP_DEBUG or not).
        // An expected answer (a missing note, an invalid note) is not an error
        // in the log: the access log line already carries its status.
        $exceptions->dontReport([NoteNotFound::class, InvalidNote::class]);
        $exceptions->shouldRenderJsonWhen(fn (Request $request, Throwable $e): bool => true);
        $exceptions->render(fn (Throwable $e, Request $request) => ErrorRenderer::render($e, $request));
    })->create();
