import re, sys
PAT = re.compile(r'<<<<<<< [^\n]*\n(.*?)\|\|\|\|\|\|\| [^\n]*\n(.*?)=======\n(.*?)>>>>>>> [^\n]*\n', re.S)
def resolve(path, fns):
    s = open(path).read()
    cs = list(PAT.finditer(s))
    assert len(cs) == len(fns), (path, len(cs))
    out, last = [], 0
    for m, fn in zip(cs, fns):
        out.append(s[last:m.start()]); out.append(fn(m.group(1), m.group(2), m.group(3))); last = m.end()
    out.append(s[last:]); open(path, 'w').write(''.join(out)); print('resolved', path)
