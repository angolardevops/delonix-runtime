<?php

namespace Tests\Architecture;

use FilesystemIterator;
use PhpToken;
use PHPUnit\Framework\TestCase;
use RecursiveDirectoryIterator;
use RecursiveIteratorIterator;

/**
 * The dependency rules of ARCHITECTURE.md as a test: it fails the moment a
 * namespace reaches in the wrong direction. Every qualified name in a file
 * counts (imports and inline \Fully\Qualified names alike).
 */
final class DependencyDirectionTest extends TestCase
{
    /** directory under app/ => name prefixes it must never reference */
    private const RULES = [
        // The capability: plain PHP. No framework, no transport, no storage
        // technology, no telemetry.
        'Notes' => ['Illuminate\\', 'Symfony\\', 'OpenTelemetry\\', 'GuzzleHttp\\', 'Monolog\\',
            'App\\Http\\', 'App\\Models\\', 'App\\Persistence\\', 'App\\Webhooks\\', 'App\\Jobs\\',
            'App\\Telemetry\\', 'App\\Logging\\', 'App\\Support\\', 'App\\Providers\\'],
        // The transport reaches the use cases through App\Notes\Notes, never
        // a model or a storage adapter.
        'Http' => ['App\\Models\\', 'App\\Persistence\\', 'App\\Jobs\\'],
        // Adapters implement ports; they do not call the transport.
        'Persistence' => ['App\\Http\\', 'App\\Webhooks\\', 'App\\Jobs\\'],
        'Models' => ['App\\Http\\', 'App\\Webhooks\\', 'App\\Jobs\\', 'App\\Notes\\'],
        'Webhooks' => ['App\\Http\\', 'App\\Persistence\\'],
        'Jobs' => ['App\\Http\\', 'App\\Persistence\\', 'App\\Models\\'],
        'Telemetry' => ['App\\Http\\', 'App\\Persistence\\', 'App\\Models\\'],
        'Support' => ['Illuminate\\', 'App\\Http\\', 'App\\Persistence\\', 'App\\Models\\'],
    ];

    public function test_dependency_direction(): void
    {
        $app = dirname(__DIR__, 2).'/app';
        $violations = [];
        $checked = 0;
        foreach (self::RULES as $dir => $forbidden) {
            $this->assertDirectoryExists("{$app}/{$dir}");
            $files = new RecursiveIteratorIterator(new RecursiveDirectoryIterator("{$app}/{$dir}", FilesystemIterator::SKIP_DOTS));
            foreach ($files as $file) {
                if ($file->getExtension() !== 'php') {
                    continue;
                }
                $checked++;
                foreach ($this->qualifiedNames((string) file_get_contents($file->getPathname())) as $name) {
                    foreach ($forbidden as $prefix) {
                        if (str_starts_with($name.'\\', $prefix)) {
                            $violations[] = sprintf('app/%s/%s references %s — forbidden by the dependency rules in ARCHITECTURE.md', $dir, $file->getFilename(), $name);
                        }
                    }
                }
            }
        }

        $this->assertGreaterThan(20, $checked, 'the test must actually read the source files');
        $this->assertSame([], $violations, implode("\n", $violations));
    }

    /** @return list<string> */
    private function qualifiedNames(string $source): array
    {
        $names = [];
        foreach (PhpToken::tokenize($source) as $token) {
            if ($token->is([T_NAME_QUALIFIED, T_NAME_FULLY_QUALIFIED])) {
                $names[] = ltrim($token->text, '\\');
            }
        }

        return array_values(array_unique($names));
    }
}
