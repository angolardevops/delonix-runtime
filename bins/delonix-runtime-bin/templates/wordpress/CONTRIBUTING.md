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
automate from Conventional Commits — none is wired up here to avoid assuming a
build step you have not run; add one when you are ready.

## Running the checks locally

```bash
# Syntax-check every PHP file in your themes/plugins — the same gate CI runs.
find wp-content -type f -name '*.php' -exec php -l {} \;
```

That is exactly what `.github/workflows/ci.yml` and `.gitlab-ci.yml` run on
every push/PR — the same `php -l` syntax check, so a green CI and a green local
run mean the same thing. It needs only a PHP CLI (`php`), never a running
WordPress or database. Add a WordPress-Coding-Standards (`phpcs`) step once you
have code worth styling.

## Static analysis (SonarQube)

`sonar-project.properties` is ready for a `sonar-scanner` run (local or via a
SonarQube/SonarCloud CI step). The `quality` profile in `compose.yaml` brings
up a local SonarQube to scan against — see README.md.
