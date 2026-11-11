import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
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
  assert.deepEqual(contract.paths["/v1/projects/{project_id}/keys"].get.security, [{ adminKey: [] }]);
  assert.deepEqual(contract.paths["/v1/projects/{project_id}/keys/{key_id}/rotate"].post.security, [{ adminKey: [] }]);
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

test("event_name discriminates every analytics event variant the server accepts", async () => {
  const { oneOf, discriminator } = contract.components.schemas.AnalyticsEvent;
  assert.equal(discriminator.propertyName, "event_name");
  const source = await readFile(new URL("../../src/event.rs", import.meta.url), "utf8");
  const accepted = [...source.match(/EVENT_NAMES: \[&str; \d+\] = \[([^\]]*)\]/)[1].matchAll(/"([^"]+)"/g)]
    .map(match => match[1]);
  assert.deepEqual(Object.keys(discriminator.mapping).sort(), accepted.sort());
  const variants = oneOf.map(variant => variant.$ref);
  assert.deepEqual(Object.values(discriminator.mapping).sort(), [...variants].sort());
  for (const [eventName, ref] of Object.entries(discriminator.mapping)) {
    const names = schemaAt({ $ref: ref }).allOf
      .map(part => part.properties?.event_name?.const)
      .filter(name => name !== undefined);
    assert.deepEqual(names, [eventName], ref);
  }
});

test("browser context is typed, versioned and accepts partial fields", () => {
  assert.deepEqual(contract.components.schemas.EventEnvelope.properties.browser_context.anyOf,
    [{ $ref: "#/components/schemas/BrowserContext" }, { type: "null" }]);
  for (const value of [{ schema_version: 1 }, { schema_version: 1, browser_name: "Firefox" },
    { schema_version: 1, page_origin: "https://example.com", page_path: "/watch", custom_tag: "kept" }]) {
    component("BrowserContext", value);
  }
  // The server reads browser fields only with schema_version 1, so the contract requires it.
  for (const value of ["Firefox", {}, { browser_name: "Firefox" }, { schema_version: 2 },
    { schema_version: 1, screen_width_px: "wide" }]) {
    assert.throws(() => component("BrowserContext", value), assert.AssertionError);
  }
});

test("the documented read concurrency matches the server defaults", async () => {
  const limits = await readFile(new URL("../../src/limits.rs", import.meta.url), "utf8");
  const perProject = limits.match(/read_concurrent_per_project: (\d+),/)[1];
  const server = limits.match(/read_concurrent: (\d+),/)[1];
  assert.match(contract.info.description,
    new RegExp(`at most ${perProject} queued/running reads per project and ${server} executing server-wide`));
});

test("removed ingestion routes are absent from the contract", () => {
  assert.equal(contract.paths["/v1/logs"], undefined);
  assert.equal(contract.paths["/v1/ingestion"], undefined);
  assert.ok(contract.paths["/v1/events"].get);
  assert.ok(contract.paths["/v1/events"].post);
  assert.equal(contract.components.schemas.IngestionStatus, undefined);
  assert.equal(contract.components.schemas.OtlpExportRequest, undefined);
});
