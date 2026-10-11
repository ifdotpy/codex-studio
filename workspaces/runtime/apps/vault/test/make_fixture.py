"""Vault objects for the JavaScript reader tests, written by the real writer with age and zstandard."""
import hashlib, json, random, subprocess, sys, tempfile
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import vault_writer
from test_vault_writer import FakeLayr, FakeS3
from vault_writer import Writer
vault_writer._codec[:] = vault_writer.compressor(stdlib=False)
out = Path(sys.argv[2]); out.mkdir(parents=True, exist_ok=True)
identity = subprocess.run(["age-keygen"], capture_output=True, text=True, check=True).stdout
secret = next(l for l in identity.splitlines() if l.startswith("AGE-SECRET-KEY-"))
recipient = subprocess.run(["age-keygen", "-y"], input=identity, capture_output=True, text=True, check=True).stdout.strip()
encrypt = lambda data: subprocess.run(["age", "-r", recipient], input=data, capture_output=True, check=True).stdout
rng = random.Random(3)
word = lambda: "".join(rng.choice("abcdefghijklmnop_") for _ in range(rng.randint(3, 10)))
source = "".join(f"def {word()}({word()}):\n    return {word()} + {i}\n" for i in range(3000)).encode()
layr, s3 = FakeLayr(), FakeS3()
w = Writer(Path(tempfile.mkdtemp()), "proj", layr, encrypt, s3)
states = []
files = {"src/app.py": (source, "100644"), "bin/tool.bin": (bytes(rng.getrandbits(8) for _ in range(50000)), "100755"),
         "link": (b"src/app.py", "120000"), "empty.txt": (b"", "100644")}
for n in range(1, 5):
    if n > 1:
        files = dict(files)
        files["src/app.py"] = (files["src/app.py"][0].replace(f" + {n * 7}\n".encode(), f" + {n * 700}\n".encode()), "100644")
        files[f"notes/{n}.md"] = (f"note {n}\n".encode(), "100644")
    if n == 3:
        files.pop("empty.txt")
    layr.commit(f"s{n}", files); w.push(f"s{n}")
    states.append({p: [hashlib.sha256(d).hexdigest(), m] for p, (d, m) in files.items()})
for key, body in s3.objects.items():
    target = out / "objects" / key
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(body)
(out / "identity.txt").write_text(secret + "\n")
(out / "expected.json").write_text(json.dumps({"recipient": recipient, "states": states}))
print("fixture", len(s3.objects), "objects")
