#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
/**
 * Print a new VAPID key pair for ADR-0012 Web Push reminders.
 *
 *   node scripts/web-push-keys.mjs
 *
 * Put the WEB_PUSH_VAPID_PRIVATE_KEY line in the server environment. The public key is shown for
 * reference only: the server derives it from the private key. Rotating the private key makes every
 * stored push subscription unusable, so viewers must turn notifications on again.
 */
import { createECDH } from "node:crypto";

const ecdh = createECDH("prime256v1");
ecdh.generateKeys();
let scalar = ecdh.getPrivateKey();
// OpenSSL can return a scalar shorter than 32 bytes when it has leading zero bytes.
if (scalar.length < 32) scalar = Buffer.concat([Buffer.alloc(32 - scalar.length), scalar]);

console.log(`WEB_PUSH_VAPID_PRIVATE_KEY=${scalar.toString("base64url")}`);
console.log(`# Public key (applicationServerKey, derived by the server): ${ecdh.getPublicKey(null, "uncompressed").toString("base64url")}`);
