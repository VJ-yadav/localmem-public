// HTTP client for the local Rust core server.
//
// Every MCP tool handler funnels through `callCore<T>` so retries,
// timeouts, and error normalization live in one place. We use `undici`
// directly (lighter than node-fetch wrappers and the only HTTP dep we
// pull in) with a short connection timeout so a missing core fails
// fast rather than hanging the MCP client.

import { request } from "undici";
import type { z } from "zod";
import { ErrorEnvelope } from "./types.js";

const DEFAULT_CORE_URL = "http://127.0.0.1:7788";
const DEFAULT_TIMEOUT_MS = 5_000;

/// Wraps a server-side `{ ok: false, error: { code, message } }` so MCP
/// callers see a single class of error regardless of where it surfaced.
export class CoreApiError extends Error {
  readonly code: string;
  readonly httpStatus: number;
  constructor(code: string, message: string, httpStatus: number) {
    super(`[${code}] ${message}`);
    this.code = code;
    this.httpStatus = httpStatus;
  }
}

export interface CoreClientOptions {
  baseUrl?: string;
  timeoutMs?: number;
}

export class CoreClient {
  private readonly baseUrl: string;
  private readonly timeoutMs: number;

  constructor(opts: CoreClientOptions = {}) {
    this.baseUrl = opts.baseUrl ?? process.env.LOCALMEM_CORE_URL ?? DEFAULT_CORE_URL;
    this.timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  }

  /// Probe `GET /health`. Returns true if the core is reachable. Used by
  /// integration tests and the optional preflight in `main()`.
  async health(): Promise<boolean> {
    try {
      const { statusCode } = await request(`${this.baseUrl}/health`, {
        method: "GET",
        bodyTimeout: this.timeoutMs,
        headersTimeout: this.timeoutMs,
      });
      return statusCode === 200;
    } catch {
      return false;
    }
  }

  /// POST `body` to `path` and parse the response against `schema`. On
  /// non-2xx with an error envelope, throws `CoreApiError`. On bad JSON
  /// or schema mismatch, throws an `Error` with a descriptive message.
  async post<T>(path: string, body: unknown, schema: z.ZodType<T>): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    let response;
    try {
      response = await request(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
        bodyTimeout: this.timeoutMs,
        headersTimeout: this.timeoutMs,
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      throw new Error(`localmem core unreachable at ${url}: ${msg}`);
    }
    return this.handleResponse(path, response, schema);
  }

  /// GET `path` (with optional querystring already on `path`) and parse
  /// the response against `schema`. Used by the MCP Resources surface
  /// (T-54). Same error-handling discipline as `post`.
  async get<T>(path: string, schema: z.ZodType<T>): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    let response;
    try {
      response = await request(url, {
        method: "GET",
        bodyTimeout: this.timeoutMs,
        headersTimeout: this.timeoutMs,
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      throw new Error(`localmem core unreachable at ${url}: ${msg}`);
    }
    return this.handleResponse(path, response, schema);
  }

  private async handleResponse<T>(
    path: string,
    response: { body: { text(): Promise<string> }; statusCode: number },
    schema: z.ZodType<T>,
  ): Promise<T> {
    const text = await response.body.text();
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      throw new Error(
        `localmem core returned non-JSON for ${path} (status ${response.statusCode}): ${text.slice(0, 200)}`,
      );
    }
    if (response.statusCode < 200 || response.statusCode >= 300) {
      const env = ErrorEnvelope.safeParse(parsed);
      if (env.success) {
        throw new CoreApiError(env.data.error.code, env.data.error.message, response.statusCode);
      }
      throw new Error(
        `localmem core returned ${response.statusCode} for ${path}: ${text.slice(0, 200)}`,
      );
    }
    const ok = schema.safeParse(parsed);
    if (!ok.success) {
      throw new Error(
        `localmem core returned unexpected shape for ${path}: ${ok.error.message}`,
      );
    }
    return ok.data;
  }
}
