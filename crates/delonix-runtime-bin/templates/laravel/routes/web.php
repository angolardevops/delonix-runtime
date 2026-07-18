<?php

use Illuminate\Foundation\Application;
use Illuminate\Support\Facades\Route;

Route::get('/', fn () => response()->json([
    'app' => config('app.name'),
    'framework' => 'Laravel '.Application::VERSION,
    'scaffolded_by' => 'delonix init --template laravel',
]));
