/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_ORGANISATION_DELETION_ENABLED?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

declare module "*.wasm?url" {
  const src: string;
  export default src;
}
