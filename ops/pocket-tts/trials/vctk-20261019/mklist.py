import json, collections, re, sys
d = json.load(open(sys.argv[1]))["members"]
fl = collections.defaultdict(dict); txt = {n for n, *_ in d if n.startswith("txt/")}
for n, s, c, crc in d:
    m = re.match(r"wav48_silence_trimmed/(\w+)/(\w+)_(\d+)_(mic[12])\.flac$", n)
    if m: fl[m.group(1)].setdefault(m.group(3), {})[m.group(4)] = s
out = []
for sp, u in sorted(fl.items()):
    items = sorted(u.items(), key=lambda kv: kv[1].get("mic2", kv[1].get("mic1")))
    lo, hi = int(len(items) * 0.60), int(len(items) * 0.95)
    band = items[lo:hi]
    pick = [band[int(i * (len(band) - 1) / 11)] for i in range(12)]
    for num, mics in dict(pick).items():
        for mic in mics:
            out.append(f"wav48_silence_trimmed/{sp}/{sp}_{num}_{mic}.flac")
        if f"txt/{sp}/{sp}_{num}.txt" in txt:
            out.append(f"txt/{sp}/{sp}_{num}.txt")
open(sys.argv[2], "w").write("\n".join(out) + "\n")
print(len(out))
