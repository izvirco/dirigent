// MUST KILL: type: one named host function per bridge kind (call, output, done, search, namespace) instead of bridge(kind, a, b, c)
export default function prelude(bridge, toolsJson, storeJson, outOfMemory) {
  // Freezing alone breaks ordinary code through the "override mistake": a data property that is
  // read-only on a prototype cannot be assigned on an instance either, so
  // this.name = "MyError" in an Error subclass would throw. Commonly overridden properties
  // become accessors whose setter defines an own property on the instance instead.
  (function lockdown() {
    const OVERRIDABLE = new Set([
      "constructor",
      "name",
      "message",
      "toString",
      "toLocaleString",
      "valueOf",
      "toJSON",
    ]);
    const seen = new Set([globalThis]);
    const queue = [];
    const add = (value) => {
      if ((typeof value === "object" && value !== null) || typeof value === "function") {
        if (!seen.has(value)) {
          seen.add(value);
          queue.push(value);
        }
      }
    };

    // Accessors from an object literal have no own prototype object, unlike function expressions,
    // whose prototype.constructor would be converted again without end.
    function allowOverride(object, key, value, enumerable) {
      const { get, set } = Object.getOwnPropertyDescriptor(
        {
          get accessor() {
            return value;
          },
          set accessor(next) {
            if (this === object) {
              throw new TypeError(
                "Cannot assign to read only property '" + String(key) + "' of a built-in",
              );
            }
            if ((typeof this !== "object" || this === null) && typeof this !== "function") return;
            Object.defineProperty(this, key, {
              value: next,
              writable: true,
              enumerable: true,
              configurable: true,
            });
          },
        },
        "accessor",
      );
      Object.defineProperty(object, key, { get, set, enumerable, configurable: false });
      add(get);
      add(set);
    }

    for (const key of Reflect.ownKeys(globalThis)) {
      const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
      add(descriptor.value);
      add(descriptor.get);
      add(descriptor.set);
      if ("value" in descriptor && descriptor.configurable) {
        Object.defineProperty(globalThis, key, { writable: false, configurable: false });
      }
    }
    add(Object.getPrototypeOf(function* () {}));
    add(Object.getPrototypeOf(async function () {}));
    add(Object.getPrototypeOf(async function* () {}));
    add(Object.getPrototypeOf(Int8Array));
    add(Object.getPrototypeOf([][Symbol.iterator]()));
    add(Object.getPrototypeOf(new Map()[Symbol.iterator]()));
    add(Object.getPrototypeOf(new Set()[Symbol.iterator]()));
    add(Object.getPrototypeOf(""[Symbol.iterator]()));
    add(Object.getPrototypeOf(/a/[Symbol.matchAll]("")));
    if (typeof Iterator === "function") {
      if (typeof Iterator.prototype.map === "function")
        add(Object.getPrototypeOf([].values().map((x) => x)));
      if (typeof Iterator.from === "function")
        add(Object.getPrototypeOf(Iterator.from({ next() {} })));
    }

    while (queue.length > 0) {
      const object = queue.pop();
      add(Object.getPrototypeOf(object));
      const descriptors = Object.getOwnPropertyDescriptors(object);
      for (const key of Reflect.ownKeys(descriptors)) {
        const descriptor = descriptors[key];
        if ("value" in descriptor) {
          add(descriptor.value);
          if (
            descriptor.writable &&
            descriptor.configurable &&
            (object === Object.prototype || OVERRIDABLE.has(key))
          ) {
            allowOverride(object, key, descriptor.value, descriptor.enumerable);
          }
        } else {
          add(descriptor.get);
          add(descriptor.set);
        }
      }
      Object.freeze(object);
    }
  })();

  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const promiseThen = Promise.prototype.then;
  const pending = new Map();
  let nextId = 1;
  let finished = false;
  const EXIT = Object.freeze({});

  // What crosses the bridge as text is read as UTF-8 on the other side: a lone surrogate (a
  // string cut in the middle of an emoji) becomes U+FFFD.
  const wellFormed = (text) => text.toWellFormed();

  // The JSON of `value` for the other side, where serde_json takes no lone surrogate either.
  // JSON.stringify writes one as the escape \udXXX (a pair it writes as the character it is),
  // so each such escape becomes U+FFFD's, in keys as in values. Every backslash in JSON begins
  // an escape: matched in order, the `u` after an escaped backslash is never taken for one.
  const ESCAPES = /\\(?:ud[89a-f][0-9a-f]{2}|(.))/g;
  function toJson(value) {
    const json = stringify(value);
    if (json === undefined || !json.includes("\\ud")) return json;
    return json.replace(ESCAPES, (escape, other) => (other === undefined ? "\\ufffd" : escape));
  }

  function done(ok, payload) {
    if (finished) return;
    finished = true;
    bridge("done", ok, payload);
  }

  // QuickJS stacks list frames only. Prefix "Name: message" like V8 so the
  // text reads the same as a Node error, and drop this prelude's frames.
  function errorText(error) {
    const head = error.message ? error.name + ": " + error.message : String(error.name);
    const frames =
      typeof error.stack === "string"
        ? error.stack
            .split("\n")
            .filter((line) => line.trim() && !line.includes("codemode-prelude.js"))
        : [];
    return [head, ...frames].join("\n");
  }

  function format(value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return errorText(value);
    // JSON.stringify turns NaN and Infinity into "null".
    if (typeof value === "number") return String(value);
    try {
      const json = stringify(value);
      return json === undefined ? String(value) : json;
    } catch {
      return String(value);
    }
  }

  function describeError(error) {
    // QuickJS throws null when it has no memory left for the error that would say so.
    if (error === null) return outOfMemory;
    try {
      return wellFormed(
        error instanceof Error ? errorText(error) : "Error: " + String(format(error)),
      );
    } catch {
      return "The script threw a value that cannot be described";
    }
  }

  const tools = Object.create(null);
  const allTools = [];
  const entries = new Map();
  for (const { name, jsName, description } of parse(toolsJson)) {
    const call = (args) =>
      new Promise((resolve, reject) => {
        let json;
        try {
          json = args === undefined ? undefined : toJson(args);
        } catch (error) {
          reject(error);
          return;
        }
        const id = nextId++;
        pending.set(id, { resolve, reject });
        bridge("call", id, name, json);
      });
    // MUST KILL: extract: a named first-wins identifier resolver that falls back to the raw name on collision
    const callName = !(jsName in tools) ? jsName : name;
    if (callName in tools) continue;
    const entry = Object.freeze({ name: callName, description });
    allTools.push(entry);
    for (const key of [callName, name]) {
      if (key in tools) continue;
      tools[key] = call;
      entries.set(key, entry);
    }
  }
  Object.freeze(tools);
  Object.freeze(allTools);

  const comparable = (name) => name.toLowerCase().replace(/[^a-z0-9]/g, "");
  const toolNames = allTools.map((tool) => tool.name);
  const toolsProxy = new Proxy(tools, {
    get(object, property, receiver) {
      if (
        typeof property !== "string" ||
        property in object ||
        property in Object.prototype ||
        property === "then" ||
        property === "toJSON"
      ) {
        return Reflect.get(object, property, receiver);
      }
      const wanted = comparable(property);
      const exact = toolNames.filter((name) => comparable(name) === wanted);
      const close =
        exact.length > 0
          ? exact
          : toolNames.filter(
              (name) =>
                wanted && (comparable(name).includes(wanted) || wanted.includes(comparable(name))),
            );
      let message = "tools." + property + " does not exist.";
      if (close.length > 0)
        message +=
          " Did you mean " +
          close
            .slice(0, 5)
            .map((name) => "tools." + name)
            .join(", ") +
          "?";
      else if (toolNames.length <= 20) message += " Available: " + toolNames.join(", ") + ".";
      message += " ALL_TOOLS lists every tool; searchTools(query) finds tools by topic.";
      throw new TypeError(message);
    },
  });

  async function searchTools(query, options) {
    if (typeof query !== "string") throw new TypeError("searchTools() expects a query string");
    const limit = options?.limit ?? 8;
    if (typeof limit !== "number" || !Number.isInteger(limit) || limit <= 0) {
      throw new TypeError("searchTools() limit must be a positive integer");
    }
    const namespace = options?.namespace ?? undefined;
    if (namespace !== undefined && typeof namespace !== "string")
      throw new TypeError("searchTools() namespace must be a string");
    const names = parse(
      bridge(
        "search",
        wellFormed(query),
        limit,
        namespace === undefined ? undefined : wellFormed(namespace),
      ),
    );
    return names.flatMap((name) => (entries.has(name) ? [entries.get(name)] : []));
  }

  async function describeTool(name) {
    if (typeof name !== "string") throw new TypeError("describeTool() expects a tool name");
    return entries.get(name)?.description;
  }

  async function describeNamespace(name) {
    if (typeof name !== "string")
      throw new TypeError("describeNamespace() expects a namespace name");
    const json = bridge("namespace", wellFormed(name));
    return json === undefined ? undefined : parse(json);
  }

  const stored = new Map(Object.entries(parse(storeJson)));
  const writes = new Map();
  let storedChars = 0;
  for (const [key, json] of stored) storedChars += key.length + json.length;

  const MAX_STORE_VALUE_CHARS = 256 * 1024;
  const MAX_STORE_TOTAL_CHARS = 1024 * 1024;
  const STORE_HINT =
    "store() is for small state such as IDs or summaries. Show images with image(), keep large data in variables, or write it to a file with a tool.";

  function keyOf(name, key) {
    if (typeof key !== "string") throw new TypeError(name + "() key must be a string");
    return wellFormed(key);
  }

  function store(given, value) {
    const key = keyOf("store", given);
    const previous = stored.has(key) ? key.length + stored.get(key).length : 0;
    if (value === undefined || value === null) {
      stored.delete(key);
      storedChars -= previous;
      writes.set(key, null);
      return;
    }
    let json;
    try {
      json = toJson(value);
    } catch (error) {
      throw new TypeError(
        "store(" + stringify(key) + ") value is not JSON-serializable: " + format(error),
      );
    }
    if (json === undefined) {
      throw new TypeError("store(" + stringify(key) + ") value is not JSON-serializable");
    }
    if (json.length > MAX_STORE_VALUE_CHARS) {
      throw new RangeError(
        "store(" +
          stringify(key) +
          ") value has " +
          json.length +
          " characters of JSON, more than the limit of " +
          MAX_STORE_VALUE_CHARS +
          ". " +
          STORE_HINT,
      );
    }
    const next = storedChars - previous + key.length + json.length;
    if (next > MAX_STORE_TOTAL_CHARS) {
      throw new RangeError(
        "store is full: stored values would exceed " +
          MAX_STORE_TOTAL_CHARS +
          " characters of JSON. Delete keys with store(key, undefined). " +
          STORE_HINT,
      );
    }
    stored.set(key, json);
    storedChars = next;
    writes.set(key, json);
  }

  function load(key) {
    const json = stored.get(keyOf("load", key));
    return json === undefined ? undefined : parse(json);
  }

  const serializeWrites = () => stringify(Object.fromEntries(writes));

  const MAX_OUTPUT_CHARS = 16 * 1024 * 1024;
  const MAX_OUTPUT_ITEMS = 100_000;
  let outputChars = 0;
  let outputItems = 0;

  // MUST KILL: extract: a failOutputLimit() that ends the script via done() before throwing, so a caught error visibly cannot resume output
  function output(kind, data, mimeType) {
    if (finished) return;
    outputChars += data.length;
    outputItems++;
    if (outputChars > MAX_OUTPUT_CHARS || outputItems > MAX_OUTPUT_ITEMS) {
      const error = new RangeError(
        "script output exceeded the limit of " +
          MAX_OUTPUT_CHARS +
          " characters or " +
          MAX_OUTPUT_ITEMS +
          " text(), image(), and console calls. " +
          "Print a summary instead, or write large data to a file with a tool.",
      );
      done(false, describeError(error));
      throw error;
    }
    bridge("output", kind, kind === "image" ? data : wellFormed(data), mimeType);
  }

  function outputText(value) {
    if (
      value === undefined ||
      value === null ||
      (typeof value !== "object" && typeof value !== "function")
    ) {
      return String(value);
    }
    const json = stringify(value);
    return json === undefined ? String(value) : json;
  }

  function text(value) {
    let rendered;
    try {
      rendered = outputText(value);
    } catch (error) {
      throw new TypeError(error instanceof Error ? error.message : String(error));
    }
    output("text", rendered);
  }

  const IMAGE_EXPECTS =
    'image expects a non-empty image URL string, an object with image_url, or an image block { type: "image", data, mimeType }';

  function imageUrl(value) {
    if (typeof value === "string") return value;
    if (typeof value !== "object" || value === null || Array.isArray(value))
      throw new TypeError(IMAGE_EXPECTS);
    if (value.image_url !== undefined) {
      if (typeof value.image_url !== "string") throw new TypeError(IMAGE_EXPECTS);
      return value.image_url;
    }
    if (typeof value.type !== "string") throw new TypeError(IMAGE_EXPECTS);
    if (value.type !== "image") {
      throw new TypeError('image only accepts image blocks, got "' + value.type + '"');
    }
    if (typeof value.data !== "string" || value.data === "")
      throw new TypeError("image expected image block data");
    if (value.data.toLowerCase().startsWith("data:")) return value.data;
    return "data:;base64," + value.data;
  }

  // Base64 of the signatures of the formats providers accept inline (PNG, JPEG except
  // JPEG-LS, GIF, "RIFF....WEBP"). Signatures start at byte 0, so their encodings are prefixes.
  const IMAGE_SIGNATURES = [
    ["image/png", /^iVBORw0KGg/],
    ["image/jpeg", /^[/]9j[/](?!9)/],
    ["image/gif", /^R0lGOD[dl]h/],
    ["image/webp", /^UklG.{8}RUJQ/],
  ];

  function image(value) {
    const url = imageUrl(value);
    if (url === "") throw new TypeError(IMAGE_EXPECTS);
    const colon = url.indexOf(":");
    const scheme = colon === -1 ? "" : url.slice(0, colon).toLowerCase();
    if (scheme === "http" || scheme === "https") {
      throw new TypeError(
        "remote image URLs are not supported in tool outputs. Pass a base64 data URI instead",
      );
    }
    const comma = url.indexOf(",");
    const header = comma === -1 ? [] : url.slice(colon + 1, comma).split(";");
    if (
      scheme !== "data" ||
      comma === -1 ||
      header.slice(1).every((part) => part.toLowerCase() !== "base64")
    ) {
      throw new TypeError("invalid image output. Pass a base64 data URI instead");
    }
    // Providers reject the whole request on a bad image, and an image in the thread is sent
    // again on every later turn. Line breaks from wrapped base64 are dropped. The declared type
    // is ignored in favor of the detected one, as providers also reject mismatches.
    const data = url.slice(comma + 1).replace(/\s+/g, "");
    if (data.length % 4 !== 0 || !/^[A-Za-z0-9+/]+={0,2}$/.test(data)) {
      throw new TypeError(
        "invalid image output. The image data is not valid base64 (truncated or corrupted?)",
      );
    }
    const head = data.slice(0, 16);
    const signature = IMAGE_SIGNATURES.find(([, pattern]) => pattern.test(head));
    if (!signature) {
      throw new TypeError(
        "invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image",
      );
    }
    output("image", data, signature[0]);
  }

  function exit() {
    done(true, serializeWrites());
    throw EXIT;
  }

  const console = {};
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    console[level] = (...args) => {
      output("console", args.map(format).join(" "));
    };
  }
  Object.freeze(console);

  const globals = {
    tools: toolsProxy,
    ALL_TOOLS: allTools,
    console,
    text,
    image,
    exit,
    store,
    load,
    searchTools,
    describeTool,
    describeNamespace,
  };
  for (const [name, value] of Object.entries(globals))
    Object.defineProperty(globalThis, name, { value, enumerable: true });

  return {
    settle(id, ok, payload) {
      const entry = pending.get(id);
      if (!entry) return;
      pending.delete(id);
      if (!ok) {
        entry.reject(new Error(payload));
        return;
      }
      let value;
      try {
        value = parse(payload);
      } catch (error) {
        entry.reject(error);
        return;
      }
      entry.resolve(value);
    },
    run(fn) {
      let promise;
      try {
        promise = fn(toolsProxy, console);
      } catch (error) {
        done(false, describeError(error));
        return;
      }
      promiseThen.call(
        promise,
        (value) => {
          try {
            if (value !== undefined) output("text", outputText(value));
          } catch (error) {
            done(false, describeError(error));
            return;
          }
          done(true, serializeWrites());
        },
        (error) => {
          done(false, describeError(error));
        },
      );
    },
    stalled() {
      if (finished || pending.size > 0) return false;
      done(
        false,
        "Error: The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here.",
      );
      return true;
    },
  };
}
