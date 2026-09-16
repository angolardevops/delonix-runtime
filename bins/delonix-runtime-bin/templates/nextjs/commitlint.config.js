// Conventional Commits config for `commitlint` — https://commitlint.js.org/.
//
// Not wired up automatically: this project has no `@commitlint/*` dependency
// (see CONTRIBUTING.md — this repo does not assume a package manager step
// you have not run). To use it locally or in CI:
//
//   pnpm add -D @commitlint/cli @commitlint/config-conventional
//   npx commitlint --edit "$1"          # e.g. from a commit-msg git hook
//
export default {
  extends: ["@commitlint/config-conventional"],
};
