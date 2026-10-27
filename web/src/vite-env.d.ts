/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** dev 模式启用 MSW（`pnpm dev:mocks`）。 */
  readonly VITE_USE_MOCKS?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
