<?php

// Syntax check (`php -l`) of every PHP file we own. `composer lint`.
$failed = 0;
$count = 0;
foreach (['app', 'bootstrap', 'config', 'database', 'routes', 'tests', 'public'] as $dir) {
    if (! is_dir($dir)) {
        continue;
    }
    $files = new RecursiveIteratorIterator(new RecursiveDirectoryIterator($dir, FilesystemIterator::SKIP_DOTS));
    foreach ($files as $file) {
        if ($file->getExtension() !== 'php') {
            continue;
        }
        $count++;
        exec(escapeshellarg(PHP_BINARY).' -l '.escapeshellarg($file->getPathname()).' 2>&1', $out, $rc);
        if ($rc !== 0) {
            $failed++;
            echo implode(PHP_EOL, $out), PHP_EOL;
        }
        $out = [];
    }
}
echo "lint: {$count} files, {$failed} with syntax errors", PHP_EOL;
exit($failed === 0 ? 0 : 1);
