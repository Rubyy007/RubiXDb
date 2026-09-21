import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiClient } from "./client";
import { ApiRequestError } from "./types";

const session = {
  baseUrl: "http://localhost:8080",
  apiKey: "test-key",
  role: "admin" as const,
  principalName: "tester",
};

function mockFetchOnce(status: number, body: unknown) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: status >= 200 && status < 300,
      status,
      statusText: "status",
      json: () => Promise.resolve(body),
    }),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("ApiClient", () => {
  it("sends the Authorization header on every request", async () => {
    mockFetchOnce(200, { ready: true, storage_state: "Healthy" });
    const client = new ApiClient(session, () => {});
    await client.ready();
    const call = (fetch as unknown as ReturnType<typeof vi.fn>).mock.calls[0];
    const [, init] = call;
    expect(init.headers.Authorization).toBe("Bearer test-key");
  });

  it("calls onUnauthorized and throws ApiRequestError on a 401", async () => {
    mockFetchOnce(401, { error: { code: "UNAUTHORIZED", message: "missing or invalid API key" } });
    const onUnauthorized = vi.fn();
    const client = new ApiClient(session, onUnauthorized);

    await expect(client.status()).rejects.toBeInstanceOf(ApiRequestError);
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it("does not call onUnauthorized on a successful request", async () => {
    mockFetchOnce(200, { storage_state: "Healthy" });
    const onUnauthorized = vi.fn();
    const client = new ApiClient(session, onUnauthorized);
    await client.status();
    expect(onUnauthorized).not.toHaveBeenCalled();
  });

  it("propagates the error code/message/detail from the response body", async () => {
    mockFetchOnce(507, {
      error: { code: "STORAGE_EXHAUSTED", message: "persistent storage is exhausted", detail: "x" },
    });
    const client = new ApiClient(session, () => {});
    try {
      await client.put("a2V5", "dmFs");
      expect.unreachable();
    } catch (err) {
      expect(err).toBeInstanceOf(ApiRequestError);
      const apiErr = err as ApiRequestError;
      expect(apiErr.status).toBe(507);
      expect(apiErr.code).toBe("STORAGE_EXHAUSTED");
      expect(apiErr.detail).toBe("x");
    }
  });

  it("builds the range query string from optional parameters", async () => {
    mockFetchOnce(200, { rows: [], truncated: false, seq_queried: null });
    const client = new ApiClient(session, () => {});
    await client.range({ startB64: "YQ==", limit: 5, asOfSeq: 42 });
    const call = (fetch as unknown as ReturnType<typeof vi.fn>).mock.calls[0];
    const [url] = call;
    expect(url).toContain("start_b64=YQ%3D%3D");
    expect(url).toContain("limit=5");
    expect(url).toContain("as_of_seq=42");
  });
});
