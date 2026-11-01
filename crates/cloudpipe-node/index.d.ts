/*!
 * cloudpipe — serverless Cloudflare tunnels for Node.js.
 *
 * Hand-written because the napi-rs auto-generator doesn't model the
 * per-event callback shape (each event variant maps to its own camelCase
 * event name with a different payload object). The shapes are kept in
 * sync with `crates/cloudpipe-node-core/src/event.rs`, which holds the
 * mapping and is unit-tested there.
 *
 * The runtime behaviour is implemented by `js/index.js`, a thin facade
 * over the native addon: it adapts napi's `(err, payload)` callback
 * shape to the single-argument `cb(payload)` documented below, gives
 * `off(event, cb)` its per-callback semantics, and replays the events
 * that fired before a handler was registered.
 */

export interface ConnectOptions {
  /** Cloudflare API token. Required. */
  token: string;

  /**
   * Base domain (e.g. `example.com`). Optional when the token has
   * access to exactly one zone — the SDK will auto-discover the zone.
   */
  domain?: string;

  /**
   * Local service protocol. One of `"http"`, `"https"`, `"tcp"`,
   * `"udp"`, `"ssh"`. Defaults to `"http"`.
   */
  protocol?: 'http' | 'https' | 'tcp' | 'udp' | 'ssh';

  /** Local port to expose. Defaults to `8080`. */
  port?: number;

  /**
   * Desired subdomain. If omitted or empty, a random one is generated
   * by the SDK.
   */
  subdomain?: string;

  /**
   * When `true`, the SDK automatically respawns `cloudflared` on the
   * same public URL if it crashes. Defaults to `false`.
   */
  autoRestart?: boolean;

  /**
   * Path to a pre-installed `cloudflared` binary. When omitted, the
   * SDK searches `PATH`, then falls back to `~/.cfp/bin/cloudflared`,
   * then downloads from GitHub releases (with the configured mirror).
   */
  cloudflaredPath?: string;

  /**
   * Override the GitHub mirror prefix used when downloading
   * `cloudflared`. Pass an empty string to disable mirroring.
   */
  githubProxy?: string;

  /**
   * Skip TLS verification when the origin is `https://` with a
   * self-signed certificate — the normal case for a local dev server.
   * Defaults to `false`.
   */
  noTlsVerify?: boolean;

  /**
   * Send this `Host` header to the origin instead of the public
   * hostname. Needed when the local service routes on its own host.
   */
  originHostHeader?: string;

  /**
   * Reconnection budget used when `autoRestart` is enabled. Omitted
   * fields fall back to the defaults (5 attempts, a 500 ms base delay
   * doubling to an 8 s ceiling).
   */
  restartPolicy?: RestartPolicyOptions;
}

/** Reconnection budget for `autoRestart`. */
export interface RestartPolicyOptions {
  /** Consecutive bootstrap failures tolerated before giving up. */
  maxAttempts?: number;
  /** Delay before the first retry, in milliseconds. */
  baseDelayMs?: number;
  /** Ceiling on any single retry delay, in milliseconds. */
  maxDelayMs?: number;
}

/** Opens a new tunnel and returns a `Listener` handle. */
export declare function connect(options: ConnectOptions): Promise<Listener>;

/** Library version, mirrored from `Cargo.toml`. */
export declare function version(): string;

/**
 * Installs a `tracing` subscriber for the SDK's internal diagnostics
 * (reconnect retries, Cloudflare backoff). Reads
 * `CFP_LOG=trace|debug|info|warn|error`; unset or `off` leaves logging
 * disabled, so the addon never writes to a consumer's stderr uninvited.
 *
 * ```ts
 * initLogging();          // with CFP_LOG=debug in the environment
 * const listener = await connect({ ... });
 * ```
 *
 * Safe to call more than once; later calls are ignored.
 */
export declare function initLogging(): void;

/**
 * Every event name `Listener#on` accepts, so a name can be validated
 * against the same list the runtime enforces.
 */
export declare function eventNames(): string[];

/** All event names the SDK may emit. */
export type ListenerEventName =
  | 'banner'
  | 'resolvingConflicts'
  | 'creatingTunnel'
  | 'ingressConfigured'
  | 'dnsCreated'
  | 'cloudflaredStarted'
  | 'restarting'
  | 'restarted'
  | 'restartGivingUp'
  | 'bootstrapFailed'
  | 'edgeConnected'
  | 'cloudflaredLog'
  | 'shuttingDown'
  | 'cleaned';

/** Severity level attached to a `cloudflaredLog` event. */
export type LogLevel = 'info' | 'warn' | 'error';

/**
 * `reason` payload on `restarting` and `shuttingDown` events. Forward
 * compatibility: any unrecognized variant comes through as
 * `{ kind: 'other', debug: '<Debug form>' }`.
 */
export type ShutdownReason =
  | { kind: 'userRequested' }
  | { kind: 'childExited' }
  | { kind: 'error'; message: string }
  | { kind: 'other'; debug: string };

/** Per-event payload shapes. Keys match {@link ListenerEventName}. */
export interface ListenerEventPayloadMap {
  banner: Record<string, never>;
  resolvingConflicts: Record<string, never>;
  creatingTunnel: { name: string };
  ingressConfigured: { protocol: string; port: number };
  dnsCreated: { fullName: string };
  cloudflaredStarted: Record<string, never>;
  restarting: { reason: ShutdownReason; attempt: number };
  restarted: { attempt: number };
  restartGivingUp: { attempts: number; lastError: string };
  bootstrapFailed: { attempt: number; error: string };
  edgeConnected: { connIndex: number; total: number };
  cloudflaredLog: { level: LogLevel; line: string };
  shuttingDown: { reason: ShutdownReason };
  cleaned: Record<string, never>;
}

/** A live Cloudflare tunnel. Constructed via {@link connect}. */
export declare class Listener {
  /** The full public URL of the tunnel (always HTTPS). */
  readonly url: string;

  /** The public hostname (`myapp.example.com`). */
  readonly fullName: string;

  /** Just the subdomain part (`myapp`). */
  readonly subdomain: string;

  /** Local port currently forwarded. */
  readonly port: number;

  /**
   * Registers a callback for a single named event. The callback
   * receives the event payload as a JSON string — `JSON.parse` it
   * inside the handler to access fields.
   *
   * Events that fired before this call are delivered to the callbacks
   * registered for that event at the moment of the call, so the startup
   * events (`dnsCreated`, `cloudflaredStarted`, …) are not lost. A
   * callback registered later for the same event does not receive them.
   *
   * Example:
   *
   * ```ts
   * listener.on('creatingTunnel', (json) => {
   *   const { name } = JSON.parse(json);
   *   console.log('creating tunnel', name);
   * });
   * ```
   *
   * Throws an `INVALID_ARGUMENT` error when `event` is not one of
   * {@link ListenerEventName} — a typo would otherwise register a
   * handler that never fires.
   */
  on(event: ListenerEventName, callback: (payload: string) => void): Listener;

  /**
   * Removes one callback for an event, or every callback for it when no
   * callback is given. Without this, a long-lived process that
   * re-registers handlers grows the registry without bound.
   */
  off(event: ListenerEventName, callback?: (payload: string) => void): Listener;

  /** Removes every registered callback. */
  removeAllListeners(): Listener;

  /**
   * Blocks until the tunnel ends on its own (cloudflared crashed with
   * auto-restart exhausted, or the session was stopped elsewhere).
   *
   * This does **not** stop the tunnel, and it does not prevent a
   * subsequent `close()` from working — a pending `wait()` is cancelled
   * by it rather than starving it.
   *
   * Several callers may wait at once; all of them are released when the
   * tunnel ends.
   */
  wait(): Promise<void>;

  /**
   * Re-points the tunnel at a different local port. The public URL is
   * unchanged, so URLs already shared keep working.
   *
   * ```ts
   * await listener.update(9090);
   * console.log(listener.port); // 9090
   * ```
   */
  update(port: number): Promise<void>;

  /**
   * Triggers a clean shutdown and waits for it to complete. Idempotent:
   * calling twice surfaces an `ALREADY_SHUT_DOWN` error.
   */
  close(): Promise<void>;
}

/**
 * Cloudpipe top-level error codes. The JS side surfaces them as a
 * `[CODE]` prefix on `error.message`, with an optional `:KIND` suffix
 * for `CLOUDFLARE_API` (e.g. `[CLOUDFLARE_API:RATE_LIMITED]`). JS callers
 * should split the prefix off the front before falling through to the
 * underlying SDK message — see {@link parseErrorCode}.
 */
export type CloudpipeErrorCode =
  | 'MISSING_CREDENTIAL'
  | 'CLOUDFLARE_API'
  | 'CLOUDFLARE_BINARY'
  | 'INVALID_SUBDOMAIN'
  | 'INVALID_DOMAIN'
  | 'SUBDOMAIN_IN_USE'
  | 'ALREADY_SHUT_DOWN'
  | 'IO'
  | 'INVALID_ARGUMENT'
  | 'OTHER';

/** Sub-kind appended after `CLOUDFLARE_API:` for API-classified errors. */
export type CloudflareApiKind =
  | 'RATE_LIMITED'
  | 'DNS_EXISTS'
  | 'AUTH_FAILED'
  | 'INVALID_TOKEN'
  | 'SERVER_ERROR'
  | 'OTHER';

/**
 * An error raised by cloudpipe.
 *
 * `code` is the stable cloudpipe taxonomy code and `kind` is the
 * sub-kind for Cloudflare API errors. Both are also present as a
 * `[CODE]` / `[CODE:KIND]` prefix on `message`.
 */
export interface CloudpipeError extends Error {
  /** Stable error code, e.g. `"CLOUDFLARE_API"`. */
  code: string;
  /** Sub-kind for Cloudflare API errors, e.g. `"RATE_LIMITED"`. */
  kind?: string;
  /** napi-rs' own status code, when it differs from `code`. */
  napiCode?: string;
}

/**
 * Extracts the `[CODE]` (or `[CODE:KIND]`) prefix from a cloudpipe
 * error message. Returns `null` when the message does not carry the
 * prefix (e.g. an `Error` thrown from outside the cloudpipe boundary).
 *
 * ```ts
 * try {
 *   await connect({ token });
 * } catch (e: any) {
 *   const code = parseErrorCode(e.message);
 *   if (code === 'CLOUDFLARE_API:RATE_LIMITED') ...
 *   else if (code === 'MISSING_CREDENTIAL') ...
 * }
 * ```
 */
export function parseErrorCode(message: string): string | null;

/**
 * The complete list of top-level error codes, as strings. Useful for
 * validating a code returned by {@link parseErrorCode} at runtime.
 */
export function errorCodes(): string[];

/** The `CLOUDFLARE_API:` sub-kind catalog, as strings. */
export function cloudflareErrorKinds(): string[];

/** Options for {@link cleanup}. */
export interface CleanupOptions {
  /** Cloudflare API token. Required. */
  token: string;
  /** Base domain (e.g. `example.com`). Optional. */
  domain?: string;
  /** Cloudflare account id. Optional. */
  account?: string;
  /** Cloudflare zone id. Optional. */
  zone?: string;
  /** Report what would be removed without removing anything. */
  dryRun?: boolean;
  /**
   * Base URL of a Cloudflare-compatible API. Intended for tests against
   * a mock server.
   */
  apiBase?: string;
}

/** One resource a cleanup run touched. */
export interface CleanupAction {
  /** `"tunnel"` or `"dnsRecord"`. */
  kind: 'tunnel' | 'dnsRecord';
  /** Cloudflare resource id. */
  id: string;
  /** Human-readable name (tunnel name or DNS hostname). */
  name: string;
}

/** The result of a cleanup run. */
export interface CleanupReport {
  /** Resources actually removed. */
  removed: CleanupAction[];
  /** Resources that would have been removed, when `dryRun` was set. */
  wouldRemove: CleanupAction[];
  /** Per-resource failures. */
  errors: string[];
}

/**
 * Reclaims dead tunnels and orphaned DNS records in one zone.
 *
 * A Node application that dies without closing its listener leaves a
 * tunnel and a CNAME behind; this is how they get reclaimed from the
 * same language.
 *
 * ```ts
 * const report = await cleanup({ token, domain: 'example.com' });
 * console.log(report.removed.length);
 * ```
 */
export declare function cleanup(options: CleanupOptions): Promise<CleanupReport>;
