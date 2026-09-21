import { describe, expect, it } from "vitest";
import { b64ToUtf8, byteLengthOfB64, isValidB64, utf8ToB64 } from "./base64";

describe("base64 utils", () => {
  it("round-trips UTF-8 text", () => {
    const text = "hello, éè world";
    expect(b64ToUtf8(utf8ToB64(text))).toBe(text);
  });

  it("matches the backend's own standard-base64 encoding", () => {
    // rubixdb-api uses base64::engine::general_purpose::STANDARD
    // (padded). "hello" -> "aGVsbG8=" is the canonical fixture used
    // throughout the backend's own manual/integration tests.
    expect(utf8ToB64("hello")).toBe("aGVsbG8=");
  });

  it("rejects invalid base64", () => {
    expect(isValidB64("not valid base64!!!")).toBe(false);
    expect(isValidB64("aGVsbG8=")).toBe(true);
    expect(isValidB64("")).toBe(true);
  });

  it("throws a clear error decoding non-UTF-8 bytes rather than mangling them", () => {
    // 0xFF is never valid as a standalone UTF-8 byte.
    const invalidUtf8B64 = btoa(String.fromCharCode(0xff));
    expect(() => b64ToUtf8(invalidUtf8B64)).toThrow(/not valid UTF-8/);
  });

  it("reports the decoded byte length", () => {
    expect(byteLengthOfB64(utf8ToB64("hello"))).toBe(5);
  });
});
