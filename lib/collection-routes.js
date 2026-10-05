// SPDX-License-Identifier: Apache-2.0
import express from "express";
import { CollectionError } from "./collections.js";

function sendError(res, error) {
  if (error instanceof CollectionError) return res.status(error.status).json({ error: error.code, message: error.message });
  return res.status(500).json({ error: "internal_error", message: "internal error" });
}

export function registerCollectionRoutes(app, { collections, resolveViewer, jsonLimit = "128kb" }) {
  if (!collections) return;
  const parseJson = express.json({ limit: jsonLimit, strict: true });
  const json = (req, res, next) => parseJson(req, res, (error) => {
    if (!error) return next();
    return res.status(error.type === "entity.too.large" ? 413 : 400).json({error: error.type === "entity.too.large" ? "body_too_large" : "invalid_body", message: "Invalid JSON request body"});
  });
  function mutationOrg(req, allowed = ["org"]) {
    if (Object.keys(req.query || {}).some(key => !allowed.includes(key)) || (req.query.org !== undefined && typeof req.query.org !== "string")) throw new CollectionError(400, "invalid_query", "unsupported collection query");
    return req.query.org;
  }
  function queryOrg(req) {
    const keys = Object.keys(req.query || {});
    if (keys.some((key) => !["org", "include"].includes(key)) || (req.query.include !== undefined && req.query.include !== "projection")) {
      throw new CollectionError(400, "invalid_query", "unsupported collection query");
    }
    return req.query.org;
  }
  function prefOrg(req) {
    const keys = Object.keys(req.query || {});
    if (keys.some((key) => key !== "org")) throw new CollectionError(400, "invalid_query", "unsupported preference query");
    return req.query.org;
  }
  async function viewerFor(req, res) {
    const viewer = await resolveViewer(req);
    if (!viewer?.email) { res.status(401).json({ error: "unauthorized", message: "Not signed in" }); return null; }
    return viewer;
  }
  app.get("/collections", async (req, res) => {
    const viewer = await viewerFor(req, res); if (!viewer) return;
    try { return res.json(collections.list(viewer, queryOrg(req))); } catch (error) { return sendError(res, error); }
  });
  app.post("/collections", json, async (req, res) => {
    const viewer = await viewerFor(req, res); if (!viewer) return;
    try { return res.status(201).json((mutationOrg(req, []), collections.create(viewer, req.body))); } catch (error) { return sendError(res, error); }
  });
  app.patch("/collections/:id", json, async (req, res) => {
    const viewer = await viewerFor(req, res); if (!viewer) return;
    try { return res.json((mutationOrg(req, []), collections.update(viewer, req.params.id, req.body))); } catch (error) { return sendError(res, error); }
  });
  app.delete("/collections/:id", async (req, res) => {
    const viewer = await viewerFor(req, res); if (!viewer) return;
    try { return res.json(collections.remove(viewer, req.params.id, mutationOrg(req))); } catch (error) { return sendError(res, error); }
  });
  for (const [method, adding] of [["post", true], ["delete", false]]) {
    app[method](`/collections/:id/memberships`, adding ? json : json, async (req, res) => {
      const viewer = await viewerFor(req, res); if (!viewer) return;
      try { return res.json((adding ? collections.addMemberships : collections.removeMemberships)(viewer, req.params.id, req.body, mutationOrg(req))); } catch (error) { return sendError(res, error); }
    });
  }
  app.get("/gallery/preferences", async (req, res) => {
    const viewer = await viewerFor(req, res); if (!viewer) return;
    try { return res.json(collections.getPreferences(viewer, prefOrg(req))); } catch (error) { return sendError(res, error); }
  });
  app.put("/gallery/preferences", json, async (req, res) => {
    const viewer = await viewerFor(req, res); if (!viewer) return;
    try { return res.json(collections.setPreferences(viewer, req.body, prefOrg(req))); } catch (error) { return sendError(res, error); }
  });
}

export default registerCollectionRoutes;
