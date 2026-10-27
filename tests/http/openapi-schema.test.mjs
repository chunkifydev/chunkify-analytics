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
  assert.deepEqual(contract.security, [{ secretKey: [] }]);
  assert.equal(contract.components.securitySchemes.publicKey.name, "X-Chunkify-Ingest-Key");
  assert.equal(contract.components.securitySchemes.secretKey.scheme, "bearer");
  assert.deepEqual(contract.paths["/v1/projects"].post.security, [{ adminKey: [] }]);
  assert.equal(contract.components.securitySchemes.adminKey.scheme, "bearer");
  assert.deepEqual(contract.paths["/healthz"].get.security, []);
  for (const path of ["/v1/events"]) {
    assert.deepEqual(contract.paths[path].post.security, [{ publicKey: [] }]);
    assert.deepEqual(contract.paths[path].options.security, []);
  }
});


test("legacy OTLP context schemas preserve null fields without allowing conflicting typed values", () => {
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

test("stored event ownership metadata uses typed schemas and supports legacy unknown keys", () => {
  const properties = contract.components.schemas.StoredEvent.properties;
  assert.equal(properties.project_id.type, "string");
  assert.deepEqual(properties.ingest_key_id.type, ["string", "null"]);
  assertSchema(properties.project_id, "customer-a", "project ID");
  assertSchema(properties.ingest_key_id, null, "legacy source key");
  assert.throws(() => assertSchema(properties.project_id, 123, "invalid project ID"), assert.AssertionError);
  assert.throws(() => assertSchema(properties.ingest_key_id, 123, "invalid source key"), assert.AssertionError);
});

test("removed ingestion routes are absent from the contract", () => {
  assert.equal(contract.paths["/v1/logs"], undefined);
  assert.equal(contract.paths["/v1/ingestion"], undefined);
  assert.ok(contract.paths["/v1/events"].get);
  assert.ok(contract.paths["/v1/events"].post);
  assert.equal(contract.components.schemas.IngestionStatus, undefined);
  assert.equal(contract.components.schemas.OtlpExportRequest, undefined);
});
