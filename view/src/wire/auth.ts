/**
 * Domain-separated mutual HMAC-SHA256 handshake authentication matching
 * `crates/apack-core/src/auth.rs` byte-for-byte.
 *
 * Pure ES2022 implementation (zero Node-only imports, works synchronously in browsers, JSDOM,
 * workers, and Node without requiring secure-context WebCrypto).
 */
import { encodeUtf8 } from './codec.ts'

const CLIENT_DOMAIN = encodeUtf8('apack-v2-client')
const SERVER_DOMAIN = encodeUtf8('apack-v2-server')

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
])

function rotr(x: number, n: number): number {
  return (x >>> n) | (x << (32 - n))
}

export function sha256(data: Uint8Array): Uint8Array {
  const bitLen = BigInt(data.length) * 8n
  const padLen = ((56 - ((data.length + 1) % 64)) + 64) % 64
  const totalLen = data.length + 1 + padLen + 8
  const padded = new Uint8Array(totalLen)
  padded.set(data, 0)
  padded[data.length] = 0x80
  const view = new DataView(padded.buffer)
  view.setUint32(totalLen - 8, Number((bitLen >> 32n) & 0xffff_ffffn), false)
  view.setUint32(totalLen - 4, Number(bitLen & 0xffff_ffffn), false)

  let h0 = 0x6a09e667
  let h1 = 0xbb67ae85
  let h2 = 0x3c6ef372
  let h3 = 0xa54ff53a
  let h4 = 0x510e527f
  let h5 = 0x9b05688c
  let h6 = 0x1f83d9ab
  let h7 = 0x5be0cd19
  const w = new Uint32Array(64)

  for (let off = 0; off < totalLen; off += 64) {
    for (let i = 0; i < 16; i += 1) {
      w[i] = view.getUint32(off + i * 4, false)
    }
    for (let i = 16; i < 64; i += 1) {
      const s0 = rotr(w[i - 15]!, 7) ^ rotr(w[i - 15]!, 18) ^ (w[i - 15]! >>> 3)
      const s1 = rotr(w[i - 2]!, 17) ^ rotr(w[i - 2]!, 19) ^ (w[i - 2]! >>> 10)
      w[i] = (w[i - 16]! + s0 + w[i - 7]! + s1) >>> 0
    }
    let a = h0, b = h1, c = h2, d = h3, e = h4, f = h5, g = h6, h = h7
    for (let i = 0; i < 64; i += 1) {
      const S1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)
      const ch = (e & f) ^ (~e & g)
      const temp1 = (h + S1 + ch + K[i]! + w[i]!) >>> 0
      const S0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)
      const maj = (a & b) ^ (a & c) ^ (b & c)
      const temp2 = (S0 + maj) >>> 0
      h = g
      g = f
      f = e
      e = (d + temp1) >>> 0
      d = c
      c = b
      b = a
      a = (temp1 + temp2) >>> 0
    }
    h0 = (h0 + a) >>> 0
    h1 = (h1 + b) >>> 0
    h2 = (h2 + c) >>> 0
    h3 = (h3 + d) >>> 0
    h4 = (h4 + e) >>> 0
    h5 = (h5 + f) >>> 0
    h6 = (h6 + g) >>> 0
    h7 = (h7 + h) >>> 0
  }

  const out = new Uint8Array(32)
  const outView = new DataView(out.buffer)
  outView.setUint32(0, h0, false)
  outView.setUint32(4, h1, false)
  outView.setUint32(8, h2, false)
  outView.setUint32(12, h3, false)
  outView.setUint32(16, h4, false)
  outView.setUint32(20, h5, false)
  outView.setUint32(24, h6, false)
  outView.setUint32(28, h7, false)
  return out
}

export function hmacSha256(key: Uint8Array, message: Uint8Array): Uint8Array {
  const keyBlock = new Uint8Array(64)
  if (key.length > 64) {
    keyBlock.set(sha256(key), 0)
  } else {
    keyBlock.set(key, 0)
  }
  const inner = new Uint8Array(64 + message.length)
  const outer = new Uint8Array(64 + 32)
  for (let i = 0; i < 64; i += 1) {
    inner[i] = keyBlock[i]! ^ 0x36
    outer[i] = keyBlock[i]! ^ 0x5c
  }
  inner.set(message, 64)
  outer.set(sha256(inner), 64)
  return sha256(outer)
}

function domainPayload(domain: Uint8Array, first: Uint8Array, second: Uint8Array): Uint8Array {
  if (first.length !== 32 || second.length !== 32) {
    throw new Error('nonces must be 32 bytes')
  }
  const msg = new Uint8Array(domain.length + 64)
  msg.set(domain, 0)
  msg.set(first, domain.length)
  msg.set(second, domain.length + 32)
  return msg
}

/** Computes the client's HMAC-SHA256 proof over `apack-v2-client || clientNonce || serverNonce`. */
export function computeClientProof(secret: Uint8Array, clientNonce: Uint8Array, serverNonce: Uint8Array): Uint8Array {
  if (secret.length !== 32) throw new Error('auth secret must be 32 bytes')
  return hmacSha256(secret, domainPayload(CLIENT_DOMAIN, clientNonce, serverNonce))
}

/** Computes the broker's mutual HMAC-SHA256 proof over `apack-v2-server || serverNonce || clientNonce`. */
export function computeServerProof(secret: Uint8Array, serverNonce: Uint8Array, clientNonce: Uint8Array): Uint8Array {
  if (secret.length !== 32) throw new Error('auth secret must be 32 bytes')
  return hmacSha256(secret, domainPayload(SERVER_DOMAIN, serverNonce, clientNonce))
}

/** Constant-time equality check over two 32-byte arrays. */
export function ctEq32(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== 32 || b.length !== 32) return false
  let diff = 0
  for (let i = 0; i < 32; i += 1) {
    diff |= a[i]! ^ b[i]!
  }
  return diff === 0
}

/** Constant-time verification of the broker's `AuthOk.serverHmac`. */
export function verifyServerProof(
  secret: Uint8Array,
  serverNonce: Uint8Array,
  clientNonce: Uint8Array,
  candidate: Uint8Array,
): boolean {
  return ctEq32(computeServerProof(secret, serverNonce, clientNonce), candidate)
}

/** Generates a cryptographically random 32-byte nonce via WebCrypto `getRandomValues`. */
export function randomNonce32(): Uint8Array {
  const nonce = new Uint8Array(32)
  globalThis.crypto.getRandomValues(nonce)
  return nonce
}
