// ESLint flat config. `next lint` was removed in Next.js 16, so ESLint runs
// directly (`pnpm lint`). The Next.js plugin is used on its own rather than
// through eslint-config-next, whose flat-config entry points exist only from
// Next.js 16 on.
import next from "@next/eslint-plugin-next";
import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

// The plugin's flat config moved between majors: `flatConfig.coreWebVitals`
// in Next.js 15, `configs["core-web-vitals"]` in Next.js 16.
const nextCoreWebVitals = next.flatConfig?.coreWebVitals ?? next.configs["core-web-vitals"];

export default tseslint.config(
  { ignores: [".next/**", "node_modules/**", ".pnpm-store/**", "coverage/**", "next-env.d.ts"] },
  ...tseslint.configs.recommended,
  nextCoreWebVitals,
  reactHooks.configs.flat.recommended,
);
