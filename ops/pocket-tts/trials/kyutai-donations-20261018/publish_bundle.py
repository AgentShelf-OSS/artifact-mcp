"""Publish a bundle.json ({path: content}) via the Artifact MCP endpoint. Token from env ARTIFACT_MCP_API_KEY."""
import json, os, sys, urllib.request
EP = "https://artifact.neilblackman.dev/mcp"
TOKEN = os.environ["ARTIFACT_MCP_API_KEY"]
def rpc(method, params, i=[0]):
    i[0] += 1
    req = urllib.request.Request(EP, data=json.dumps({"jsonrpc": "2.0", "id": i[0], "method": method, "params": params}).encode(),
        headers={"authorization": "Bearer " + TOKEN, "content-type": "application/json", "accept": "application/json, text/event-stream", "user-agent": "pocket-audition-publisher/1"})
    with urllib.request.urlopen(req, timeout=180) as r:
        body = json.loads(r.read())
    if "error" in body:
        sys.exit(f"{method} error: {json.dumps(body['error'])[:500]}")
    res = body["result"]
    if res.get("isError"):
        sys.exit(f"{method} tool error: {json.dumps(res)[:800]}")
    return res
def call(name, args):
    res = rpc("tools/call", {"name": name, "arguments": args})
    return res.get("structuredContent") or json.loads(next(c["text"] for c in res["content"] if c["type"] == "text"))
rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pocket-audition-publisher", "version": "1"}})
if sys.argv[1] == "--categories":
    print(json.dumps(call("list_categories", {"org": "homelab"}))[:1500]); sys.exit()
files = json.loads(open(sys.argv[1]).read())
out = call("publish_bundle", {"files": files, "entry": "index.html", "title": "Pocket voice audition",
    "description": "Listen to donated Pocket voices and choose which ones join the Listen voice picker.",
    "category": "Family", "org": "homelab", "artifact_key": "voices/pocket-donated-audition"})
print(json.dumps({k: out.get(k) for k in ("id", "url", "revision", "artifact_key", "created")}))
