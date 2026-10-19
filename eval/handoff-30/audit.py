import json,hashlib,collections
from pathlib import Path
R=Path(__file__).resolve().parent/'measurement'
d=json.loads((R/'results.json').read_text());cases=d['cases'];checks=[];tokens=[]
for c in cases:
 ok=False;reason=c.get('failure')
 if 'input' in c and 'receiver_turns' in c:
  inp=c['input'];tokens.append(inp['token'])
  subtotal=inp['qty']*inp['unit_cents'];shipping=175 if subtotal<3000 else 0
  expected=dict(token=inp['token'],subtotal_cents=subtotal,shipping_cents=shipping,total_cents=subtotal+shipping)
  ts=c['receiver_turns'];answers='\n'.join('\n'.join(b.get('text','') for b in t.get('blocks',[]) if b.get('type')=='text') for t in ts if t.get('role')=='assistant')
  tools=[b for t in ts if t.get('role')=='assistant' for b in t.get('blocks',[]) if b.get('type')=='tool_use']
  try:parsed=json.loads(answers.strip())
  except ValueError:parsed=None
  producer_text='\n'.join(b.get('text','') for t in c['producer_turns'] for b in t.get('blocks',[]) if b.get('type')=='text')
  ok=parsed==expected and not tools and inp['token'] not in producer_text
  assert ok==c['success'],f"driver/auditor disagree at {(c['round'],c['edge'])}"
 checks.append(dict(round=c['round'],edge=c['edge'],source=c['from'],target=c['to'],success=ok,reason=reason))
assert len(tokens)==len(set(tokens)),'random token repeated'
counts=collections.defaultdict(lambda:{'passed':0,'attempted':0})
for c in checks:
 x=counts[c['source']+' -> '+c['target']];x['attempted']+=1;x['passed']+=c['success']
result=dict(status=d['status'],planned=d['planned'],attempted=len(cases),passed=sum(c['success'] for c in checks),rate=sum(c['success'] for c in checks)/len(cases) if cases else None,by_direction=dict(counts),checks=checks,unique_inputs=len(tokens),results_sha256=hashlib.sha256((R/'results.json').read_bytes()).hexdigest())
(R/'audit.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
print(json.dumps({k:v for k,v in result.items() if k!='checks'},ensure_ascii=False,indent=2))
