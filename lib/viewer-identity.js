// Stable, opaque viewer handles for per-viewer state. The audit key never leaves this module.
import crypto from "node:crypto";

function keyFrom(value) {
  const raw = String(value || "").trim();
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(raw) || raw.length % 4 !== 0) return null;
  const key = Buffer.from(raw, "base64");
  return key.length === 32 && key.toString("base64") === raw ? key : null;
}

export function deriveViewerId(email, auditKey = process.env.AUDIT_LEDGER_HMAC_KEY) {
  const key = keyFrom(auditKey);
  const identity = String(email || "").trim().toLowerCase();
  if (!key || !identity) return null;
  const namespace = crypto.createHmac("sha256", key).update("artifact-viewer-id", "utf8").digest();
  return crypto.createHmac("sha256", namespace).update(`viewer-id:${identity}`, "utf8").digest("hex").slice(0, 16);
}

export function deriveViewerName(email) {
  const local = (String(email || "").split("@", 1)[0] || String(email || "")).replace(/[\u0000-\u001f\u007f-\u009f]/gu, "");
  const words = local.split(/[._+]/).filter(Boolean).map((word) => {
    const chars = [...word.toLowerCase()];
    return chars.length ? chars[0].toUpperCase() + chars.slice(1).join("") : "";
  }).join(" ");
  return [...words].slice(0, 40).join("");
}
