'use strict'

/**
 * The public entry point of `@xinggao/cloudpipe`.
 *
 * The native addon is a thin, untyped shell over the Rust SDK. This
 * facade is where the JavaScript-facing contract lives:
 *
 *  - `on(event, cb)` calls `cb(payload)`, with a single argument. napi's
 *    `ThreadsafeFunction` calls `cb(null, payload)`, so the native
 *    binding cannot offer that shape on its own.
 *  - `off(event, cb)` removes one callback. The native binding can only
 *    clear a whole event.
 *  - Events that fire before a handler is registered are delivered as
 *    soon as one is. The startup events fire while the tunnel is being
 *    created — before `connect()` even resolves — so without this they
 *    would be unreachable from JavaScript.
 *  - Errors carry `code` and `kind` properties instead of only a
 *    `[CODE]` prefix on the message.
 *  - A platform without a prebuilt binary gets an actionable message
 *    instead of "Cannot find native binding".
 */

const SUPPORTED_TARGETS = [
  'linux-x64-gnu',
  'darwin-arm64',
  'darwin-x64',
  'win32-x64-msvc',
]

/** How many events are held per name before the oldest is dropped. */
const PENDING_CAP = 64

let native
try {
  native = require('../npm/index.js')
} catch (cause) {
  const target = `${process.platform}-${process.arch}`
  throw new Error(
    `cloudpipe does not ship a prebuilt binary for ${target}.\n` +
      `Supported targets: ${SUPPORTED_TARGETS.join(', ')}.\n` +
      'Build from source with `npm run build:local`.',
    { cause },
  )
}

/**
 * Splits a cloudpipe error message into its code and kind.
 *
 * @param {string} message
 * @returns {{ code: string, kind?: string } | null}
 */
function splitCode(message) {
  const raw = native.parseErrorCode(message)
  if (!raw) return null
  const [code, kind] = raw.split(':')
  return kind ? { code, kind } : { code }
}

/**
 * Adds `code` / `kind` to an error thrown by the native binding.
 *
 * The `[CODE] ` prefix stays in the message so existing consumers that
 * parse it keep working.
 *
 * @param {unknown} err
 * @returns {unknown}
 */
function decorate(err) {
  if (!(err instanceof Error)) return err
  const parsed = splitCode(err.message)
  if (!parsed) return err
  // napi-rs sets `code` to its own status string; keep it under a
  // different name so nothing is lost when we take the slot.
  if (err.code !== undefined && err.code !== parsed.code) {
    err.napiCode = err.code
  }
  err.code = parsed.code
  if (parsed.kind) err.kind = parsed.kind
  return err
}

/**
 * Wraps a native listener so the JavaScript API matches what the
 * README promises.
 *
 * @param {any} listener
 * @param {string[]} eventNames
 */
function wrapListener(listener, eventNames) {
  /** @type {Map<string, Set<Function>>} */
  const handlers = new Map()
  /** @type {Map<string, string[]>} */
  const pending = new Map()

  function deliver(set, payload) {
    for (const cb of Array.from(set)) {
      try {
        cb(payload)
      } catch {
        // One throwing handler must not stop the others.
      }
    }
  }

  function dispatch(name, payload) {
    const set = handlers.get(name)
    if (set && set.size > 0) {
      deliver(set, payload)
      return
    }
    // Nobody is listening yet. Hold the event: the startup events fire
    // before any JavaScript could possibly have registered a handler.
    let queue = pending.get(name)
    if (!queue) {
      queue = []
      pending.set(name, queue)
    }
    if (queue.length >= PENDING_CAP) queue.shift()
    queue.push(payload)
  }

  // Register natively for every known event up front. This is what
  // drains the SDK's own pending buffer (the events emitted while the
  // tunnel was being created) into this facade instead of dropping it.
  for (const name of eventNames) {
    listener.on(name, (err, payload) => {
      if (err) return
      dispatch(name, payload)
    })
  }

  return {
    get url() {
      return listener.url
    },
    get fullName() {
      return listener.fullName
    },
    get subdomain() {
      return listener.subdomain
    },
    get port() {
      return listener.port
    },
    /**
     * Registers a callback for a named event.
     *
     * @param {string} event
     * @param {(payload: string) => void} callback
     */
    on(event, callback) {
      if (typeof callback !== 'function') {
        throw new TypeError('on(event, callback) requires a function')
      }
      let set = handlers.get(event)
      if (!set) {
        set = new Set()
        handlers.set(event, set)
      }
      set.add(callback)
      // Hand over the events that arrived before this listener.
      const queue = pending.get(event)
      if (queue && queue.length > 0) {
        pending.delete(event)
        deliver(set, queue.shift())
        for (const payload of queue) deliver(set, payload)
      }
      return this
    },
    /**
     * Removes one callback, or every callback for the event when no
     * callback is given.
     *
     * @param {string} event
     * @param {Function} [callback]
     */
    off(event, callback) {
      const set = handlers.get(event)
      if (!set) return this
      if (callback === undefined) {
        set.clear()
      } else {
        set.delete(callback)
      }
      if (set.size === 0) handlers.delete(event)
      return this
    },
    /**
     * Removes every registered callback.
     */
    removeAllListeners() {
      handlers.clear()
      return this
    },
    /** Blocks until the tunnel exits on its own. */
    async wait() {
      await listener.wait()
    },
    /** Re-points the tunnel at a different local port. */
    async update(port) {
      await listener.update(port)
    },
    /** Triggers a clean shutdown. */
    async close() {
      await listener.close()
    },
  }
}

/**
 * Opens a new tunnel.
 *
 * @param {any} options
 */
async function connect(options) {
  try {
    const listener = await native.connect(options)
    return wrapListener(listener, native.eventNames())
  } catch (err) {
    throw decorate(err)
  }
}

/**
 * Reclaims dead tunnels and orphaned DNS records.
 *
 * @param {any} options
 */
async function cleanup(options) {
  try {
    return await native.cleanup(options)
  } catch (err) {
    throw decorate(err)
  }
}

module.exports = {
  connect,
  cleanup,
  Listener: native.Listener,
  version: native.version,
  errorCodes: native.errorCodes,
  eventNames: native.eventNames,
  parseErrorCode: native.parseErrorCode,
  cloudflareErrorKinds: native.cloudflareErrorKinds,
  initLogging: native.initLogging,
}
