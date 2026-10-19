import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

export const contractPath = new URL("../../openapi.json", import.meta.url);
export const contract = JSON.parse(await readFile(contractPath, "utf8"));
const schemaId = "https://chunkify.example/collector-contract";
const ajv = new Ajv2020({ strict: false, allErrors: true });
addFormats(ajv);
// Preserve local component references, including recursive OTLP AnyValue schemas.
ajv.addSchema({ $id: schemaId, components: contract.components });
const validators = new Map();

export function assertSchema(schema, value, label) {
  let validate = validators.get(schema);
  if (!validate) {
    const qualified = schema.$ref?.startsWith("#") ? { ...schema, $ref: `${schemaId}${schema.$ref}` } : schema;
    validate = ajv.compile(qualified);
    validators.set(schema, validate);
  }
  assert.equal(validate(value), true, `${label}: ${ajv.errorsText(validate.errors, { separator: "; " })}`);
}

export function component(name, value) {
  assertSchema({ $ref: `${schemaId}#/components/schemas/${name}` }, value, name);
}

export function schemaAt(schema) {
  if (!schema.$ref) return schema;
  assert.ok(schema.$ref.startsWith("#/"));
  return schema.$ref.slice(2).split("/").reduce((value, key) => value[key.replaceAll("~1", "/").replaceAll("~0", "~")], contract);
}

export function assertRequest(path, method, body) {
  const schema = contract.paths[path][method].requestBody.content["application/json"].schema;
  assertSchema(schema, body, `${method.toUpperCase()} ${path} request`);
}

export async function assertResponse(path, method, response, expectedStatus) {
  assert.equal(response.status, expectedStatus, await response.clone().text());
  const declared = contract.paths[path][method].responses[response.status];
  assert.ok(declared, `${method} ${path}: undocumented status ${response.status}`);
  assert.equal(response.headers.get("cache-control"), "no-store");
  for (const [name, header] of Object.entries(declared.headers ?? {})) {
    const value = response.headers.get(name);
    if (value !== null) assertSchema(header.schema, value, `${name} response header`);
  }
  if (!declared.content) {
    assert.equal(await response.text(), "");
    return;
  }
  const mediaType = response.headers.get("content-type")?.split(";")[0];
  assert.ok(declared.content[mediaType], `Undocumented media type ${mediaType}`);
  const body = mediaType === "application/json" ? await response.json() : await response.text();
  assertSchema(declared.content[mediaType].schema, body, `${method.toUpperCase()} ${path} ${response.status}`);
  return body;
}
