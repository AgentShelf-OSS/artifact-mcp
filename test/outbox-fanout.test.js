import test from "node:test";
import assert from "node:assert/strict";
import crypto from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";

const importDataDir = mkdtempSync(path.join(tmpdir(), "artifact-outbox-fanout-import-"));
process.env.DATA_DIR = importDataDir;
const { buildDeliveryEnvelopeV1, canonicalDeliveryEnvelopeBytes, decodeDeliveryEnvelopeV1, discordRequestBodyBytes, stableDeliveryEventId } = await import("../lib/delivery-envelope.js");
const { createOutboxFanout } = await import("../lib/outbox-fanout.js");

test.after(() => rmSync(importDataDir, { recursive: true, force: true }));

function payload() { return { title:"Report", url:"", description:"A report", uploaderLabel:"Ada", category:"Docs", revision:2, bytes:128 }; }
function fixture() { const db=new Database(":memory:");db.pragma("foreign_keys=ON");migrateDatabase(db);db.prepare("INSERT INTO orgs (name) VALUES ('acme')").run();let n=0;return { db, fanout:createOutboxFanout({db,now:()=>100,id:()=>`outbox-${++n}`}) }; }
function webhook(db,id,events) { db.prepare("INSERT INTO org_webhooks (id,org,url,events,created_at) VALUES (?,'acme','https://discord.com/api/webhooks/123/testing-token',?,'2026-01-01 00:00:00')").run(id,events); }

test("canonical v1 envelope has stable IDs, bytes, hashable payload, no preview or webhook secret", () => {
 const event_id=stableDeliveryEventId("acme","published","artifact:artifact-1:2");assert.equal(event_id,"delivery:v1:7bcad03fcf3d78ad6ec5d6dbe903dead096b9d437bb8b91c00d47a0c94e4dba4");const first=buildDeliveryEnvelopeV1({event_id,tenant:"acme",event:"published",payload:payload()});const second=buildDeliveryEnvelopeV1({event_id,tenant:"acme",event:"published",payload:payload()});assert.deepEqual(canonicalDeliveryEnvelopeBytes(first),canonicalDeliveryEnvelopeBytes(second));const text=canonicalDeliveryEnvelopeBytes(first).toString();assert.equal(text,'{"version":1,"event_id":"delivery:v1:7bcad03fcf3d78ad6ec5d6dbe903dead096b9d437bb8b91c00d47a0c94e4dba4","tenant":"acme","event_type":"published","provider":"discord","payload":{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Report","fields":[{"name":"Publisher","value":"Ada","inline":true},{"name":"Category","value":"Docs","inline":true},{"name":"Revision","value":"2","inline":true},{"name":"Size","value":"128 B","inline":true}],"description":"A report"}]}}');assert.doesNotMatch(text,/preview\.png|api\/webhooks|testing-token/);
});
test("strict v1 decode is bound and emits only the nested Discord request body", () => {
 const event_id=stableDeliveryEventId("acme","published","artifact:artifact-1:2");const envelope=buildDeliveryEnvelopeV1({event_id,tenant:"acme",event:"published",payload:payload()});const bytes=canonicalDeliveryEnvelopeBytes(envelope);const decoded=decodeDeliveryEnvelopeV1(bytes,{tenant:"acme",event:"published",event_id,payload_sha256:cryptoHash(bytes)});const body=discordRequestBodyBytes(decoded).toString();assert.equal(body,'{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Report","fields":[{"name":"Publisher","value":"Ada","inline":true},{"name":"Category","value":"Docs","inline":true},{"name":"Revision","value":"2","inline":true},{"name":"Size","value":"128 B","inline":true}],"description":"A report"}]}');assert.match(body,/^\{"embeds":/);assert.doesNotMatch(body,/"version"|"event_id"|"tenant"|"provider"/);
});
test("transactional fanout filters subscriptions and maps only opaque references", () => {
 const {db,fanout}=fixture();webhook(db,"wh-one","published,updated");webhook(db,"wh-two","published");webhook(db,"wh-other","feedback");const envelope=buildDeliveryEnvelopeV1({event_id:"delivery:v1:test",tenant:"acme",event:"published",payload:payload()});const rows=fanout.fanout({envelope,tenant:"acme",event:"published"});assert.equal(rows.length,2);assert.deepEqual(rows.map((row)=>row.target_key),["wh-one","wh-two"]);const stored=db.prepare("SELECT target_key,secret_ref,payload FROM provider_delivery_outbox ORDER BY id").all();assert.deepEqual(stored.map((row)=>[row.target_key,row.secret_ref]),[["wh-one","webhook:wh-one"],["wh-two","webhook:wh-two"]]);assert.ok(stored.every((row)=>!Buffer.from(row.payload).toString().includes("webhooks")));db.close();
});
test("zero subscribers and tampered IDs do not create a partial fanout", () => {
 const {db,fanout}=fixture();const none=buildDeliveryEnvelopeV1({event_id:"delivery:v1:none",tenant:"acme",event:"published",payload:payload()});assert.deepEqual(fanout.fanout({envelope:none,tenant:"acme",event:"published"}),[]);webhook(db,"wh-good","published");webhook(db,"bad/id","published");const bad=buildDeliveryEnvelopeV1({event_id:"delivery:v1:bad",tenant:"acme",event:"published",payload:payload()});assert.throws(()=>fanout.fanout({envelope:bad,tenant:"acme",event:"published"}));assert.equal(db.prepare("SELECT COUNT(*) FROM provider_delivery_outbox").pluck().get(),0);db.close();
});
test("tenant fanout capacity refusal leaves no subscriber batch", () => {
 const {db,fanout}=fixture();fanout.outbox.enqueueMany(Array.from({length:1000},(_,n)=>({event_id:`fill-${n}`,tenant:"acme",event_type:"published",target_key:`target-${n}`,secret_ref:`webhook:target-${n}`,payload:Buffer.from("{}")})));webhook(db,"wh-cap","published");const cap=buildDeliveryEnvelopeV1({event_id:"delivery:v1:cap",tenant:"acme",event:"published",payload:payload()});assert.throws(()=>fanout.fanout({envelope:cap,tenant:"acme",event:"published"}),/capacity/);assert.equal(db.prepare("SELECT COUNT(*) FROM provider_delivery_outbox").pluck().get(),1000);db.close();
});
test("forged, tampered, and mismatched envelopes have no network-ready body or fanout", () => {
 const {db,fanout}=fixture();webhook(db,"wh-one","published");const event_id=stableDeliveryEventId("acme","published","artifact:artifact-1:2");const valid=buildDeliveryEnvelopeV1({event_id,tenant:"acme",event:"published",payload:payload()});const bytes=canonicalDeliveryEnvelopeBytes(valid);const text=bytes.toString();const altered=Buffer.from(text.replace("Report","Altered"));const reorderedTop=Buffer.from(text.replace('{"version":1,"event_id":"delivery:','{"event_id":"delivery:').replace('","tenant":"acme"','","version":1,"tenant":"acme"'));const reorderedNested=Buffer.from(text.replace('{"color":3120756,"author":{"name":"acme"},"title":"Report"','{"title":"Report","color":3120756,"author":{"name":"acme"}'));assert.throws(()=>decodeDeliveryEnvelopeV1(altered,{tenant:"acme",event:"published",event_id,payload_sha256:cryptoHash(bytes)}));assert.throws(()=>decodeDeliveryEnvelopeV1(Buffer.from("[]"),{tenant:"acme",event:"published",event_id}));assert.throws(()=>decodeDeliveryEnvelopeV1(Buffer.from(`${text.slice(0,-1)},"unexpected":true}`),{tenant:"acme",event:"published",event_id}));assert.throws(()=>decodeDeliveryEnvelopeV1(bytes,{tenant:"other",event:"published",event_id}));assert.throws(()=>decodeDeliveryEnvelopeV1(reorderedTop,{tenant:"acme",event:"published",event_id}));assert.throws(()=>decodeDeliveryEnvelopeV1(reorderedNested,{tenant:"acme",event:"published",event_id}));assert.throws(()=>fanout.fanout({envelope:{...valid},tenant:"acme",event:"published"}));assert.throws(()=>fanout.fanout({envelope:valid,tenant:"acme",event:"deleted"}));assert.equal(db.prepare("SELECT COUNT(*) FROM provider_delivery_outbox").pluck().get(),0);db.close();
});

function cryptoHash(bytes) { return crypto.createHash("sha256").update(bytes).digest("hex"); }
