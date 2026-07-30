// SPDX-License-Identifier: Apache-2.0
// Secret-free subscriber lookup and atomic enqueue fanout; no decryption or network I/O.
import { createOutboxStore } from "./outbox.js";
import { canonicalDeliveryEnvelopeBytes, deliveryEnvelopeHash, validateDeliveryEnvelopeV1 } from "./delivery-envelope.js";

export function createOutboxFanout({ db, now, id } = {}) {
  const outbox = createOutboxStore({ db, now, id });
  const subscribers = db.prepare("SELECT id FROM org_webhooks WHERE org = ? AND instr(',' || events || ',', ',' || ? || ',') > 0 ORDER BY created_at ASC, id ASC");
  function fanoutInTransaction({ envelope, tenant, event, durability_intent_id = null } = {}) {
    validateDeliveryEnvelopeV1(envelope, { tenant, event });
    const bytes = canonicalDeliveryEnvelopeBytes(envelope); const digest = deliveryEnvelopeHash(envelope);
    const inputs = subscribers.all(envelope.tenant, envelope.event_type).map(({ id: target_key }) => ({ event_id: envelope.event_id, tenant: envelope.tenant, event_type: envelope.event_type, target_key, secret_ref: `webhook:${target_key}`, payload: bytes, payload_sha256: digest, durability_intent_id }));
    return outbox.enqueueManyInTransaction(inputs);
  }
  return { fanoutInTransaction, fanout: db.transaction(fanoutInTransaction).immediate, outbox };
}
