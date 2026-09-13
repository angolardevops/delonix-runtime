# __NAME__ (NestJS)

A NestJS service scaffolded by `delonix init --template nestjs` — TypeScript,
decorators, modular structure, **pnpm**, health probes, a Delonix manifest.

## Run it locally (pnpm)

```bash
corepack enable
pnpm install
pnpm start:dev
curl localhost:__PORT__/api/v1/health/live
```

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
