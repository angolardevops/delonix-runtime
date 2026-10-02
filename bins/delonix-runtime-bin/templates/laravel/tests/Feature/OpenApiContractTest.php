<?php

namespace Tests\Feature;

use Illuminate\Support\Facades\Route;
use Tests\TestCase;

/**
 * api/openapi.yaml and the registered routes must describe the same
 * operations. Add a route without documenting it (or the reverse) and this
 * fails, naming the operation.
 */
final class OpenApiContractTest extends TestCase
{
    public function test_openapi_matches_the_routes(): void
    {
        $documented = $this->documentedOperations();
        $registered = $this->registeredOperations();

        $this->assertNotEmpty($documented);
        $this->assertSame([], array_values(array_diff($registered, $documented)), 'routes missing from api/openapi.yaml');
        $this->assertSame([], array_values(array_diff($documented, $registered)), 'operations in api/openapi.yaml with no route');
    }

    /** @return list<string> "METHOD /path" */
    private function registeredOperations(): array
    {
        $operations = [];
        foreach (Route::getRoutes()->getRoutes() as $route) {
            $uri = '/'.ltrim($route->uri(), '/');
            if (! str_starts_with($uri, '/api/')) {
                continue;
            }
            foreach ($route->methods() as $method) {
                if ($method !== 'HEAD') {
                    $operations[] = $method.' '.$uri;
                }
            }
        }
        sort($operations);

        return $operations;
    }

    /**
     * Reads the `paths:` block of the YAML by indentation — enough for this
     * file's own layout, with no YAML dependency.
     *
     * @return list<string>
     */
    private function documentedOperations(): array
    {
        $operations = [];
        $inPaths = false;
        $path = null;
        foreach (file(base_path('api/openapi.yaml'), FILE_IGNORE_NEW_LINES) ?: [] as $line) {
            if (preg_match('/^(\S.*):\s*$/', $line, $m) === 1) {
                $inPaths = $m[1] === 'paths';

                continue;
            }
            if (! $inPaths) {
                continue;
            }
            if (preg_match('/^  (\/\S*):\s*$/', $line, $m) === 1) {
                $path = $m[1];
            } elseif ($path !== null && preg_match('/^    (get|post|put|patch|delete):\s*$/', $line, $m) === 1) {
                $operations[] = strtoupper($m[1]).' '.$path;
            }
        }
        sort($operations);

        return $operations;
    }
}
