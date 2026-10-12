import { beforeEach, expect, it, vi } from "vitest";

const { initMock } = vi.hoisted(() => ({ initMock: vi.fn() }));

vi.mock("../wasm/sotto_wasm.js", () => ({
  default: initMock,
  aead_open: vi.fn(),
  format_decode_key: vi.fn(),
  kdf_derive_master_key: vi.fn(),
  name_decrypt_env: vi.fn(),
  name_decrypt_org: vi.fn(),
  name_decrypt_project: vi.fn(),
  scheme_version: vi.fn(),
  share_open: vi.fn(),
  share_passphrase_key: vi.fn(),
  share_seal: vi.fn(),
  vault_decrypt_name: vi.fn(),
  vault_decrypt_org: vi.fn(),
  vault_decrypt_project: vi.fn(),
  vault_decrypt_value: vi.fn(),
  vault_grant_key: vi.fn(),
  vault_open_grant: vi.fn(),
  vault_rewrap_data_key: vi.fn(),
}));
vi.mock("../wasm/sotto_wasm_bg.wasm?url", () => ({ default: "/mock/sotto.wasm" }));

beforeEach(() => {
  vi.resetModules();
  initMock.mockReset();
});

it("shares pending initialisation and caches success", async () => {
  let resolveInit!: () => void;
  initMock.mockImplementation(
    () => new Promise<void>((resolve) => { resolveInit = resolve; }),
  );
  const { loadWasm } = await import("../wasm");

  const first = loadWasm();
  const second = loadWasm();
  expect(initMock).toHaveBeenCalledTimes(1);
  expect(first).toBe(second);

  resolveInit();
  await Promise.all([first, second]);
  await loadWasm();
  expect(initMock).toHaveBeenCalledTimes(1);
});

it("clears a rejected initialisation so the next call can retry", async () => {
  initMock.mockRejectedValueOnce(new Error("temporary failure")).mockResolvedValueOnce(undefined);
  const { loadWasm } = await import("../wasm");

  const first = loadWasm();
  const second = loadWasm();
  await expect(first).rejects.toThrow("temporary failure");
  await expect(second).rejects.toThrow("temporary failure");
  expect(initMock).toHaveBeenCalledTimes(1);

  await expect(loadWasm()).resolves.toBeUndefined();
  await expect(loadWasm()).resolves.toBeUndefined();
  expect(initMock).toHaveBeenCalledTimes(2);
});
