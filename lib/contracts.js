// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function typeName(schema) {
  return ["object", "integer", "array"].includes(schema.type) ? `an ${schema.type}` : `a ${schema.type}`;
}

function validateNode(schema, value, path, errors) {
  if (schema.type === "array") {
    if (!Array.isArray(value)) {
      errors.push(`${path} must be ${typeName(schema)}`);
      return;
    }
    if (schema.minItems !== undefined && value.length < schema.minItems) {
      errors.push(`${path} must contain at least ${schema.minItems} item${schema.minItems === 1 ? "" : "s"}`);
    }
    if (schema.items) {
      value.forEach((item, index) => validateNode(schema.items, item, `${path}.${index}`, errors));
    }
    return;
  }

  if (schema.type === "object") {
    if (!isPlainObject(value)) {
      errors.push(`${path} must be ${typeName(schema)}`);
      return;
    }

    for (const key of schema.required || []) {
      if (!Object.hasOwn(value, key)) errors.push(`${path ? `${path}.` : ""}${key} is required`);
    }

    const properties = schema.properties || {};
    const unknown = Object.keys(value).filter((key) => !Object.hasOwn(properties, key));
    if (schema.additionalProperties === false) {
      for (const key of unknown) errors.push(`${path ? `${path}.` : ""}${key} is not allowed`);
    }

    for (const [key, childSchema] of Object.entries(properties)) {
      if (Object.hasOwn(value, key)) {
        validateNode(childSchema, value[key], path ? `${path}.${key}` : key, errors);
      }
    }
    if (isPlainObject(schema.additionalProperties)) {
      for (const key of unknown) {
        validateNode(schema.additionalProperties, value[key], path ? `${path}.${key}` : key, errors);
      }
    }
    return;
  }

  if (schema.type === "integer" && (typeof value !== "number" || !Number.isInteger(value))) {
    errors.push(`${path} must be ${typeName(schema)}`);
  } else if (schema.type && schema.type !== "integer" && typeof value !== schema.type) {
    errors.push(`${path} must be ${typeName(schema)}`);
  }
}

export function validateSchemaInput(schema, value) {
  const errors = [];
  validateNode(schema, value, "", errors);
  return errors.map((error) => error.replace(/^ must/, "arguments must"));
}

// Page encoded bytes, never JavaScript UTF-16 code units. A caller-supplied offset inside a
// multi-byte sequence advances to the next character boundary; an end inside one retreats to the
// preceding boundary so `limit` remains a hard maximum.
export function pageUtf8(value, requestedOffset = 0, limit = 65536) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(value, "utf8");
  let offset = Math.min(requestedOffset, bytes.length);
  while (offset < bytes.length && (bytes[offset] & 0xc0) === 0x80) offset += 1;

  let end = Math.min(offset + limit, bytes.length);
  while (end > offset && end < bytes.length && (bytes[end] & 0xc0) === 0x80) end -= 1;

  return {
    bytes_total: bytes.length,
    offset,
    bytes_returned: end - offset,
    truncated: end < bytes.length,
    content: bytes.subarray(offset, end).toString("utf8")
  };
}

export function applyUtf8Edits(value, edits) {
  const source = Buffer.isBuffer(value) ? value : Buffer.from(value, "utf8");
  const resolved = edits.map((edit, index) => {
    const number = index + 1;
    const hasFind = Object.hasOwn(edit, "find");
    const hasRange = Object.hasOwn(edit, "offset") || Object.hasOwn(edit, "length");
    if (hasFind === hasRange) {
      throw new Error(`edit ${number} must use either find/replace or offset/length/replace`);
    }
    if (hasFind) {
      if (!edit.find) throw new Error(`edit ${number} find must not be empty`);
      const needle = Buffer.from(edit.find, "utf8");
      const matches = [];
      for (let from = 0; from <= source.length - needle.length;) {
        const found = source.indexOf(needle, from);
        if (found < 0) break;
        matches.push(found);
        from = found + 1;
      }
      if (matches.length !== 1) {
        throw new Error(`edit ${number} find matched ${matches.length} times; expected exactly once`);
      }
      return {
        number,
        start: matches[0],
        end: matches[0] + needle.length,
        replacement: Buffer.from(edit.replace, "utf8")
      };
    }

    for (const field of ["offset", "length"]) {
      if (!Number.isSafeInteger(edit[field]) || edit[field] < 0) {
        throw new Error(`edit ${number} ${field} must be a non-negative integer`);
      }
    }
    if (edit.offset > source.length || edit.length > source.length - edit.offset) {
      throw new Error(`edit ${number} range exceeds content length of ${source.length} bytes`);
    }
    const end = edit.offset + edit.length;
    if (edit.offset < source.length && (source[edit.offset] & 0xc0) === 0x80) {
      throw new Error(`edit ${number} offset ${edit.offset} is not a UTF-8 boundary`);
    }
    if (end < source.length && (source[end] & 0xc0) === 0x80) {
      throw new Error(`edit ${number} range end ${end} is not a UTF-8 boundary`);
    }
    return {
      number,
      start: edit.offset,
      end,
      replacement: Buffer.from(edit.replace, "utf8")
    };
  });

  resolved.sort((left, right) => left.start - right.start || left.number - right.number);
  for (let index = 1; index < resolved.length; index += 1) {
    const previous = resolved[index - 1];
    const current = resolved[index];
    if (current.start < previous.end) {
      throw new Error(`edits ${previous.number} and ${current.number} overlap in the original content`);
    }
  }

  const chunks = [];
  let cursor = 0;
  for (const edit of resolved) {
    chunks.push(source.subarray(cursor, edit.start), edit.replacement);
    cursor = edit.end;
  }
  chunks.push(source.subarray(cursor));
  const content = Buffer.concat(chunks);
  if (content.equals(source)) throw new Error("Patch did not change artifact content");
  return { content: content.toString("utf8"), bytes_before: source.length, bytes_after: content.length };
}

export function parseReactionInput(value) {
  if (!isPlainObject(value)) throw new Error("Reaction body must be a JSON object.");

  const keys = Object.keys(value);
  if (keys.length === 0) throw new Error("Reaction body must include favorite or vote.");
  const unknown = keys.find((key) => key !== "favorite" && key !== "vote");
  if (unknown) throw new Error(`Unknown reaction field: ${unknown}`);

  const update = {};
  if (Object.hasOwn(value, "favorite")) {
    if (![true, false, 0, 1].includes(value.favorite)) {
      throw new Error("favorite must be true, false, 0, or 1.");
    }
    update.favorite = value.favorite ? 1 : 0;
  }
  if (Object.hasOwn(value, "vote")) {
    if (![-1, 0, 1].includes(value.vote)) throw new Error("vote must be -1, 0, or 1.");
    update.vote = value.vote;
  }
  return update;
}
