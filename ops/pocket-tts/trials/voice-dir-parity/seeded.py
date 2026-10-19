import hashlib, json, sys
sys.path.insert(0, "/app")
import torch
import server
TEXT = "The lighthouse keeper counted the ships at dusk, and wrote each name in the ledger."
VOICES = ["pocket_alba", "pocket_vera", "pocket_estelle", "pocket_andy", "pocket_karen_savage2", "pocket_kristin_hughes_expressive",
          "pocket_laura_caldwell", "pocket_simon_evers", "pocket_andrea", "pocket_darius", "pocket_hannah", "pocket_bedfordshire"]
out = {"voices_list_sha": hashlib.sha256(json.dumps(server.VOICES, ensure_ascii=False).encode()).hexdigest(), "voice_count": len(server.VOICES), "audio": {}}
for v in VOICES:
    hashes = []
    for _ in range(2):
        torch.manual_seed(1234); server.ACTIVE_BUDGET = server.GenerationBudget(120)
        a = server.model.generate_audio(server.voices[v], TEXT, copy_state=True).detach().cpu().numpy()
        hashes.append(hashlib.sha256(a.tobytes()).hexdigest()[:16] + ":%d" % a.shape[0])
    out["audio"][v] = hashes
print(json.dumps(out))
