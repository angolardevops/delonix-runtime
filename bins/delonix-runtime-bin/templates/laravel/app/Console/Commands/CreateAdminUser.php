<?php

namespace App\Console\Commands;

use App\Models\User;
use Illuminate\Console\Command;
use Illuminate\Support\Facades\Hash;

class CreateAdminUser extends Command
{
    protected $signature = 'admin:create-user {--name=Admin User} {--email=admin@example.com} {--password=}';

    protected $description = 'Create a default admin user with auto-generated password if not provided';

    public function handle(): int
    {
        $name = $this->option('name');
        $email = $this->option('email');
        $password = $this->option('password') ?: bin2hex(random_bytes(8));

        if (User::where('email', $email)->exists()) {
            $this->info("Admin user '$email' already exists");
            return 0;
        }

        User::create([
            'name' => $name,
            'email' => $email,
            'password' => Hash::make($password),
            'email_verified_at' => now(),
        ]);

        $this->line('');
        $this->line(str_repeat('=', 60));
        $this->line('Laravel Admin User Created');
        $this->line(str_repeat('=', 60));
        $this->line("Email:    $email");
        $this->line("Password: $password");
        $this->line(str_repeat('=', 60));
        $this->line('');

        return 0;
    }
}
