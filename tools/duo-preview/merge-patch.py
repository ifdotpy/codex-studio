import subprocess, sys, os, shutil, re
patch, base_rev, repo, tmp = sys.argv[1:5]
text = open(patch).read()
files = re.findall(r'^diff --git a/(\S+) b/\S+', text, re.M)
shutil.rmtree(tmp, ignore_errors=True)
for side in ('base', 'theirs'):
    for f in files:
        p = os.path.join(tmp, side, f); os.makedirs(os.path.dirname(p), exist_ok=True)
        r = subprocess.run(['git', '-C', repo, 'show', f'{base_rev}:{f}'], capture_output=True)
        open(p, 'wb').write(r.stdout if r.returncode == 0 else b'')
new = set(re.findall(r'^diff --git a/(\S+) b/\S+\nnew file', text, re.M))
for f in new: os.remove(os.path.join(tmp, 'theirs', f))
r = subprocess.run(['git', 'apply', '--unsafe-paths', patch], cwd=os.path.join(tmp, 'theirs'), capture_output=True, text=True)
print('apply-to-base rc', r.returncode, r.stderr[:500])
if r.returncode: sys.exit(1)
for f in files:
    ours = os.path.join(repo, f); theirs = os.path.join(tmp, 'theirs', f)
    if not os.path.exists(ours):
        os.makedirs(os.path.dirname(ours), exist_ok=True); shutil.copy(theirs, ours); print('added', f); continue
    r = subprocess.run(['git', 'merge-file', '-L', 'ours', '-L', 'base', '-L', 'theirs', ours, os.path.join(tmp, 'base', f), theirs])
    print('clean' if r.returncode == 0 else f'CONFLICT({r.returncode})', f)
