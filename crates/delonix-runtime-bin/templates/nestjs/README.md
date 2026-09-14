# __NAME__ (NestJS __TEMPLATE_VERSION__)

A NestJS service scaffolded by `delonix init -t nestjs -v __TEMPLATE_VERSION__`
— every `@nestjs/*` package pinned to `^__TEMPLATE_VERSION__` (core, common,
platform-express and the CLI move together — a mixed-major NestJS install is
not a supported combination), TypeScript, decorators, modular structure,
**pnpm**, health probes, a Delonix manifest.

## Run it locally (pnpm)

```bash
corepack enable
pnpm install
pnpm start:dev
curl localhost:__PORT__/api/v1/health/live
pnpm add <package>       # add a dependency
```

## Choosing a NestJS version

Regenerate with `-v <major.minor.patch>` to pin another line, e.g.
`delonix init -t nestjs -v 10.0.0 --force .` (`--force`: `init` never
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
