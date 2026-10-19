import json, sys
W = sys.argv[1]
m = json.load(open(f"{W}/out/export-manifest.json"))
reserved = json.load(open(f"{W}/reserved.json"))
prep = ["andy", "karen_savage2", "kristin_hughes_expressive", "laura_caldwell", "simon_evers", "andrea", "darius", "hannah", "bedfordshire"]
jobs = []
for v in reserved:
    jobs.append({"voice": v, "kind": "prepared", "prompt": f"/voices/{v}.safetensors"} if v in prep else {"voice": v, "kind": "preset", "prompt": v})
for r in m["results"]:
    jobs.append({"voice": r["voice"], "kind": "candidate", "prompt": "/out/states/%s.safetensors" % r["voice"]})
json.dump(jobs, open(f"{W}/synth-jobs.json", "w"))
ids = [r["voice"] for r in m["results"]]
print(len(jobs), len(set(ids)), max(map(len, ids)), [i for i in ids if i[-2:-1] == "_" or i.startswith("donor")])
