<?php

namespace App\Console\Commands;

use App\Support\ConfigValidator;
use Illuminate\Console\Command;

/**
 * `php artisan app:check-config` — run by the container entrypoint (and
 * `composer serve`) before the server listens. Exit 1 with every problem.
 */
final class CheckConfig extends Command
{
    protected $signature = 'app:check-config';

    protected $description = 'Validate the service configuration; exit 1 listing every problem';

    public function handle(): int
    {
        $errors = ConfigValidator::errors(
            (array) config('app'),
            (array) config('service'),
            (string) config('logging.channels.json.level', 'info'),
        );
        if ($errors === []) {
            $this->line('configuration: OK');

            return self::SUCCESS;
        }
        $this->error('configuration:');
        foreach ($errors as $error) {
            $this->line('  - '.$error);
        }

        return self::FAILURE;
    }
}
