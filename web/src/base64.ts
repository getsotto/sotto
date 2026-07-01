// Base64 decoding for the two encodings we receive: standard (server `enc_blob`/`passphrase_salt`)
// and URL-safe-no-pad (the fragment key the CLI puts after `#`).

export function standardB64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

export function urlSafeB64ToBytes(b64url: string): Uint8Array {
  let b64 = b64url.replace(/-/g, "+").replace(/_/g, "/");
  while (b64.length % 4 !== 0) {
    b64 += "=";
  }
  return standardB64ToBytes(b64);
}
