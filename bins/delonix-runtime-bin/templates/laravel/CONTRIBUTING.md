# Contributing to __NAME__

## Branching model (git-flow)

- `main` — always deployable; every commit here is a release.
- `develop` — integration branch; feature work merges here first.
- `feature/<name>` — branched from `develop`, merged back into `develop`.
- `release/<x.y.z>` — branched from `develop` when preparing a release, merged into both `main` and `develop`.
- `hotfix/<x.y.z>` — branched from `main` for urgent fixes, merged into both `main` and `develop`.

No `git-flow` CLI required — the model above is just branch naming and merge
direction; `git checkout -b feature/my-thing develop` is all it takes.

## Commit messages (Conventional Commits)

Every commit message follows <https://www.conventionalcommits.org/>:

```
<type>(<scope>): <description>

[optional body]
[optional footer(s)]
```

`type` is one of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`. A `!` after the type/scope (`feat!: ...`) or a
`BREAKING CHANGE:` footer marks a breaking change.

## Versioning (SemVer)

This project follows <https://semver.org/>: `MAJOR.MINOR.PATCH`.

- A commit with `fix:` bumps PATCH.
- A commit with `feat:` bumps MINOR.
- A commit with `!` or a `BREAKING CHANGE:` footer bumps MAJOR.

That mapping is exactly what tools like `semantic-release`/`standard-version`
automate from Conventional Commits — none is wired up here to avoid assuming
a package manager step you have not run; add one when you are ready.

## Running the checks locally

```bash
composer install
cp .env.example .env && php artisan key:generate
touch database/database.sqlite
php artisan test
```

These are the same steps `.github/workflows/ci.yml` and `.gitlab-ci.yml` run
on every push/PR.

## Static analysis (SonarQube)

`sonar-project.properties` is ready for a `sonar-scanner` run (local or via a
SonarQube/SonarCloud CI step) — widen `sonar.sources` once you add your own
`app/`/`config/` code.
