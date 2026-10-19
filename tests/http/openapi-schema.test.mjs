import assert from "node:assert/strict";
import test from "node:test";
import { fileURLToPath } from "node:url";
import SwaggerParser from "@apidevtools/swagger-parser";
import { contractPath, contract, component, assertSchema, schemaAt } from "./openapi-support.mjs";

test("OpenAPI document and every component schema validate", async () => {
  await SwaggerParser.validate(fileURLToPath(contractPath));
  for (const name of Object.keys(contract.components.schemas)) {
    // Schema compilation happens before value validation. A value mismatch is
    // expected here; malformed schemas and unresolved references must throw.
    try { component(name, null); } catch (error) {
      if (!(error instanceof assert.AssertionError)) throw error;
    }
  }
  for (const item of Object.values(contract.paths)) for (const operation of Object.values(item)) {
    for (const parameter of operation.parameters ?? []) {
      const resolved = schemaAt(parameter);
      assert.ok(resolved.schema, `${operation.operationId}: parameter schema is required`);
    }
    for (const media of Object.values(operation.requestBody?.content ?? {})) {
      if (media.example !== undefined) assertSchema(media.schema, media.example, `${operation.operationId} example`);
    }
    for (const response of Object.values(operation.responses)) for (const media of Object.values(response.content ?? {})) {
      if (media.example !== undefined) assertSchema(media.schema, media.example, `${operation.operationId} response example`);
    }
  }
});

test("the contract separates public ingestion from private read authorization", () => {
  assert.deepEqual(contract.security, [{ readToken: [] }]);
  assert.equal(contract.components.securitySchemes.ingestKey.name, "X-Chunkify-Ingest-Key");
  assert.equal(contract.components.securitySchemes.readToken.scheme, "bearer");
  assert.deepEqual(contract.paths["/healthz"].get.security, []);
  for (const path of ["/v1/events", "/v1/logs"]) {
    assert.deepEqual(contract.paths[path].post.security, [{ ingestKey: [] }]);
    assert.deepEqual(contract.paths[path].options.security, []);
  }
});


test("OTLP schemas allow ignored null fields without allowing conflicting typed values", () => {
  component("OtlpResource", null);
  component("OtlpScope", null);
  for (const value of [null, {}, { stringValue: null }, { stringValue: null, bytesValue: null },
    { stringValue: "kept", boolValue: null }, { arrayValue: { values: null } },
    { kvlistValue: { values: null } }, { unknownExtension: "preserved" }]) {
    component("OtlpAttributeValue", value);
  }
  for (const value of [{ stringValue: "one", boolValue: true }, { bytesValue: "YQ==" }, { stringValue: 7 }]) {
    assert.throws(() => component("OtlpAttributeValue", value), assert.AssertionError);
  }
});
