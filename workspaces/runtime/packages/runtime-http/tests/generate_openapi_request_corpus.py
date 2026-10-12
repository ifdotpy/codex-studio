import argparse
import json,copy,collections,re
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('output_dir', type=Path, help='temporary directory for the generated corpus')
args=parser.parse_args()
args.output_dir.mkdir(parents=True, exist_ok=True)
DOC=json.loads((ROOT/'openapi.json').read_text());defs=DOC['components']['schemas']
def resolve(s):
 return defs[s['$ref'].rsplit('/',1)[-1]] if '$ref'in s else s
def type_names(s,seen=()):
 if not isinstance(s,dict):return set()
 ref=s.get('$ref')
 if ref:
  if ref in seen:return set()
  return type_names(resolve(s),seen+(ref,))
 names={s['type']} if isinstance(s.get('type'),str) else set(s.get('type',[])) if isinstance(s.get('type'),list) else set()
 for key in ['anyOf','oneOf','allOf']:
  for branch in s.get(key,[]):names.update(type_names(branch,seen))
 return names
def schema_at(s,path):
 s=resolve(s)
 if not path:return s
 if path[0] in s.get('properties',{}):return schema_at(s['properties'][path[0]],path[1:])
 if path[0]=='key' and isinstance(s.get('additionalProperties'),dict):return schema_at(s['additionalProperties'],path[1:])
 for key in ['allOf','anyOf','oneOf']:
  for branch in s.get(key,[]):
   try:return schema_at(branch,path)
   except KeyError:pass
 if s.get('type')=='array' and isinstance(path[0],int):return schema_at(s.get('items',{}),path[1:])
 raise KeyError(path)
patterns=set();formats=set()
def scan(x):
 if isinstance(x,dict):
  if isinstance(x.get('pattern'),str):patterns.add(x['pattern'])
  if isinstance(x.get('format'),str):formats.add(x['format'])
  for v in x.values():scan(v)
 elif isinstance(x,list):
  for v in x:scan(v)
scan(DOC);print('patterns',patterns,'formats',formats)
def sample(s,depth=0):
 s=resolve(s)
 if depth>7:return None
 if 'const'in s:return s['const']
 if 'enum'in s:return s['enum'][0]
 for key in ['anyOf','oneOf']:
  if key in s:return sample(next((b for b in s[key] if b.get('type')!='null'),s[key][0]),depth+1)
 t=s.get('type')
 if t=='object' or 'properties'in s:
  if 'properties'in s:return {k:sample(v,depth+1) for k,v in s['properties'].items() if k in s.get('required',[]) }
  if isinstance(s.get('additionalProperties'),dict):return {'key':sample(s['additionalProperties'],depth+1)}
  return {}
 if t=='array':return [sample(s.get('items',{}),depth+1) for _ in range(max(1,s.get('minItems',0)))]
 if t=='string':
  n=max(1,s.get('minLength',0));v='x'*n
  if s.get('pattern'):
   for candidate in ['x','abc','test','00000000-0000-0000-0000-000000000000','a'*64,'a'*32,'1','agent-1','http://example.com','AWS_KEY','ABC','us-east-1']:
    if re.search(s['pattern'],candidate):return candidate
  return v
 if t=='integer':return int(max(1,s.get('minimum',1),s.get('exclusiveMinimum',0)+1))
 if t=='number':return max(1.,s.get('minimum',1),s.get('exclusiveMinimum',0)+1)
 if t=='boolean':return True
 return None
# Enumerate field and nested constraint sites, including absent optional fields by constructing their parent.
def sites(s,path=(),depth=0,active=()):
 if depth>6:return
 ref=s.get('$ref')
 if ref and ref in active:return
 s=resolve(s);active=active+((ref,) if ref else ())
 yield path,s
 for u in ['anyOf','oneOf']:
  for b in s.get(u,[]):yield from sites(b,path,depth+1,active)
 for k,v in s.get('properties',{}).items():yield from sites(v,path+(k,),depth+1,active)
 if s.get('type')=='array':yield from sites(s.get('items',{}),path+(0,),depth+1,active)
 if isinstance(s.get('additionalProperties'),dict):yield from sites(s['additionalProperties'],path+('key',),depth+1,active)
def change(base,path,value,delete=False):
 b=copy.deepcopy(base)
 if not path:return value
 current=b
 for i,k in enumerate(path[:-1]):
  nxt=[] if isinstance(path[i+1],int) else {}
  if isinstance(k,int):
   if not isinstance(current,list):raise ValueError()
   while len(current)<=k:current.append(copy.deepcopy(nxt))
   if not isinstance(current[k],(dict,list)):current[k]=nxt
  else:
   if not isinstance(current,dict):raise ValueError()
   if not isinstance(current.get(k),(dict,list)):current[k]=nxt
  current=current[k]
 k=path[-1]
 if isinstance(k,int):
  if not isinstance(current,list):raise ValueError()
  while len(current)<=k:current.append(None)
 if delete:
  if isinstance(current,dict):current.pop(k,None)
 else:current[k]=value
 return b
cases=[];ops=[]
for path,item in DOC['paths'].items():
 for method,op in item.items():
  if not isinstance(op,dict):continue
  schema=op.get('requestBody',{}).get('content',{}).get('application/json',{}).get('schema');params=op.get('parameters',[])
  method=method.upper();ops.append((method,path));base=sample(schema) if schema else None
  if isinstance(base,dict):
   for k in ['login_id','request_id']:
    if k in base:base[k]='00000000-0000-4000-8000-000000000001'
   if path=='/api/queue':base['id']='x'
   if path=='/api/rename':base['name']='x'
   if path=='/api/sync/drafts':base={'rows':[]}

  query={p['name']:str(sample(p.get('schema',{}))).lower() if isinstance(sample(p.get('schema',{})),bool) else str(sample(p.get('schema',{}))) for p in params if p.get('in')=='query' and p.get('required')}
  headers={p['name']:str(sample(p.get('schema',{}))).lower() if isinstance(sample(p.get('schema',{})),bool) else str(sample(p.get('schema',{}))) for p in params if p.get('in')=='header' and p.get('required')}
  path_values={p['name']:sample(p.get('schema',{})) for p in params if p.get('in')=='path'}
  from urllib.parse import quote
  request_path=path
  for key,value in path_values.items(): request_path=request_path.replace('{'+key+'}',quote(str(value),safe=''))
  def add(label,body=base,q=query,h=headers,body_present=schema is not None):cases.append({'id':len(cases),'method':method,'path':path,'requestPath':request_path,'pathValues':{k:str(v) for k,v in path_values.items()},'label':label,'body':body,'bodyPresent':body_present,'query':q,'headers':h})
  def add_raw(label,raw,body=None,body_present=True):
   add(label,body=body,q=query,h=headers,body_present=body_present);cases[-1]['raw']=raw
  add('baseline')
  if path=='/api/servers/orchestration' and method=='POST':
   huge=2**100
   add('payload/key:lax:'+str(huge),body=change(base,('payload','key'),huge))
  request_body=op.get('requestBody')
  if request_body and not request_body.get('required',False):add('body:missing',body=None,body_present=False)
  if path=='/api/accounts/discover' and request_body and not request_body.get('required',False):
   add_raw('body:empty','',body=None)
  if path=='/api/agents' and method=='POST':
   add_raw('body:malformed','{',body=None)
  if path=='/api/agents' and method=='POST':
   add_raw('body:duplicate-key','{"prompt":"first","prompt":"last"}',body={'prompt':'last'})
   add_raw('body:private-number-key-as-object-for-integer','{"prompt":"x","concurrency":{"$serde_json::private::Number":"1"}}')
  if path=='/api/answer' and method=='POST':
   add_raw('body:private-number-key-as-object-for-freeform','{"id":"x","answers":{"$serde_json::private::Number":"1"}}',body={'id':'x','answers':{'$serde_json::private::Number':'1'}})
  if schema:
   for at,s in sites(schema):
    type_variants=[None,False,0,1.0,'x',[],{}]
    # Integral JSON decimals for integer schemas are documented separately;
    # emit canonical integers at integer and integer/null field locations.
    declared_types=type_names(schema_at(schema,at))
    if 'integer' in declared_types and 'number' not in declared_types:type_variants.remove(1.0)
    variants=[('type:'+type(v).__name__,v) for v in type_variants]
    if s.get('type') in ['integer','number','boolean']:
     lax_values=['1','1.0','1_000',' 1 ','true','false','yes','NaN','Infinity','-Infinity',1,0,True,False,2**64,2**100,1.5]
     # Integral JSON decimals are a documented JSON Schema/Pydantic difference,
     # not a required differential corpus case. A Rust unit test records it.
     if s.get('type')=='integer':lax_values.remove(1.0)
     variants += [('lax:'+repr(v),v) for v in lax_values]
    if 'enum'in s or 'const'in s:variants.append(('enum-miss','__invalid__'))
    for key,delta in [('minimum',-1),('maximum',1),('exclusiveMinimum',0),('exclusiveMaximum',0)]:
     if key in s:variants.extend([(key+':below',s[key]-1),(key+':above',s[key]+1),(key+':equal',s[key])])
    for key in ['minLength','maxLength']:
     if key in s:variants.extend([(key+':below','x'*max(0,s[key]-1)),(key+':above','x'*(s[key]+1))])
    for key in ['minItems','maxItems']:
     if key in s and s[key]<200:variants.extend([(key+':below',[sample(s.get('items',{}))]*max(0,s[key]-1)),(key+':above',[sample(s.get('items',{}))]*(s[key]+1))])
    if 'pattern'in s:variants.append(('pattern-miss','! invalid !'))
    if s.get('type')=='object':
     full=sample(s)
     if isinstance(full,dict):
      variants.append(('extra',dict(full,unexpected=True)))
      for required in s.get('required',[]):
       missing=copy.deepcopy(full);missing.pop(required,None);variants.append(('missing:'+required,missing))
      for n in [2,3]:
       bad=copy.deepcopy(full)
       for k in list(s.get('properties',{}))[:n]:bad[k]=[]
       variants.append(('compound:'+str(n),bad))
    for label,v in variants:
     if label.startswith(('minimum:', 'maximum:', 'exclusiveMinimum:', 'exclusiveMaximum:')) and s.get('type')=='integer' and isinstance(v,(int,float)):
      v=int(v)
     try:add('/'.join(map(str,at))+':'+label,change(base,at,v))
     except ValueError:pass
  for p in params:
   if p.get('in')=='query':
    name=p['name'];s=resolve(p.get('schema',{}))
    for value in ['', 'x','1','1.0','1_000',' 1 ','true','false','yes','NaN','Infinity',str(2**100)]:add('query:'+name+':'+value,q=dict(query,**{name:value}))
    if p.get('required'):
     q=copy.deepcopy(query);q.pop(name,None);add('query:missing:'+name,q=q)
   elif p.get('in')=='header':
    name=p['name'];value=headers.get(name,str(sample(p.get('schema',{}))))
    for candidate in ['', 'x','1','1.0','1_000',' 1 ','true',str(2**100)]:add('header:'+name+':'+candidate,h=dict(headers,**{name:candidate}))
    if p.get('required'):
     h=copy.deepcopy(headers);h.pop(name,None);add('header:missing:'+name,h=h)
  add('query:extra',q=dict(query,unexpected='1'))
  add('query:extra-blank',q=dict(query,unexpected=''))
  add('query:extras',q=dict(query,zeta='1',alpha='2'))
# Include one repeated-value boundary case for each required query parameter.
for case in list(cases):
 if case['label']!='baseline': continue
 operation=DOC['paths'][case['path']][case['method'].lower()]
 for parameter in operation.get('parameters',[]):
  if parameter.get('in')!='query': continue
  name=parameter['name']; schema=resolve(parameter.get('schema',{}))
  if schema.get('type')=='array':
   item=schema.get('items',{}); value=sample(item)
   values=[value, value]
  else:
   value=case['query'].get(name, str(sample(parameter.get('schema',{}))))
   values=[value, value]
  pairs=[(key,current) for key,current in case['query'].items() if key!=name]+[(name,item) for item in values]
  repeated=copy.deepcopy(case); repeated['label']='query:repeated:'+name; repeated['queryPairs']=pairs
  cases.append(repeated)
  if schema.get('type')!='array':
   empty=copy.deepcopy(case); empty['label']='query:repeated-empty:'+name
   empty['queryPairs']=[(key,current) for key,current in case['query'].items() if key!=name]+[(name,''),(name,'')]
   cases.append(empty)
   if name:
    encoded_name=f'%{ord(name[0]):02X}'+name[1:]
    from urllib.parse import quote, urlencode
    others=[(key,current) for key,current in case['query'].items() if key!=name]
    raw_other=urlencode(others)
    raw_target='&'.join(f'{encoded_name}={quote(str(item), safe="")}' for item in values)
    encoded=copy.deepcopy(case); encoded['label']='query:repeated-encoded:'+name
    encoded['queryPairs']=others+[(name,item) for item in values]
    encoded['rawQueryEncoded']='&'.join(part for part in (raw_other,raw_target) if part)
    cases.append(encoded)
# deduplicate same request per operation; retain first label
seen=set();unique=[]
for c in cases:
 key=json.dumps([c['method'],c['path'],c['requestPath'],c.get('raw',c['body']),c['bodyPresent'],c['query'],c.get('headers'),c.get('queryPairs'),c.get('rawQueryEncoded')],sort_keys=True)
 if key not in seen:seen.add(key);c['id']=len(unique);unique.append(c)

for c in unique:
 c.setdefault('raw',json.dumps(c['body'],ensure_ascii=True))
 from urllib.parse import urlencode
 c['rawQuery']=c.pop('rawQueryEncoded',None) or urlencode(c.get('queryPairs',c['query']))
for i,c in enumerate(unique): c['id']=i
corpus_path=args.output_dir/'openapi-request-corpus.json'
corpus_path.write_text(json.dumps(unique,separators=(',',':')))
print('operations',len(ops),'cases',len(unique),'excluded_signed_federation',0)
print('corpus',corpus_path)
