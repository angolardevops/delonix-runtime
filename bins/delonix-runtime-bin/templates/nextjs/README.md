# __NAME__ (Next.js __TEMPLATE_VERSION__)

A Next.js (App Router) app scaffolded by
`delonix init -t nextjs -v __TEMPLATE_VERSION__` — pinned to
`next: ^__TEMPLATE_VERSION__`, TypeScript, React 19, Route Handlers for
health, **pnpm**, a Delonix manifest.

## Run it locally (pnpm)

```bash
corepack enable
pnpm install
pnpm dev
curl localhost:__PORT__/api/v1/health/live
pnpm add <package>       # add a dependency
```

## Choosing a Next.js version

Regenerate with `-v <major.minor.patch>` to pin another line, e.g.
`delonix init -t nextjs -v 14.2.0 --force .` (`--force`: `init` never
overwrites without it). Omit `-v` and it falls back to this template's own
default (`__TEMPLATE_VERSION__`, from `template.meta`).

## Build & deploy with Delonix

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/api/v1/health/live
```

## Contributing

CI (`.github/workflows/ci.yml` for GitHub, `.gitlab-ci.yml` for GitLab) runs
`pnpm build` on every push and PR (no test script ships yet — see
CONTRIBUTING.md). `sonar-project.properties` is ready for a SonarQube/
SonarCloud scan, and `commitlint.config.js` for Conventional Commits (once
you `pnpm add -D @commitlint/cli @commitlint/config-conventional`). See
[CONTRIBUTING.md](CONTRIBUTING.md) for the branching model (git-flow) and
versioning (SemVer).
