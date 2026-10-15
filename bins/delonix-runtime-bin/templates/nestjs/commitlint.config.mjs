// Conventional Commits config for `commitlint` — https://commitlint.js.org/.
//
// Not wired up automatically: this project has no `@commitlint/*` dependency
// (see CONTRIBUTING.md — this repo does not assume a package manager step you
// have not run). `.mjs` because this package is CommonJS (`tsc` emits CJS for
// Nest's decorators), so a bare `.js` here would be parsed as CommonJS and the
// `export default` below would not load. To use it locally or in CI:
//
//   pnpm add -D @commitlint/cli @commitlint/config-conventional
//   pnpm exec commitlint --edit "$1"          # e.g. from a commit-msg git hook
//
const config = {
  extends: ["@commitlint/config-conventional"],
};

export default config;
