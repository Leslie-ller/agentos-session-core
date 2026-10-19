import json,time,uuid,re,urllib.request,urllib.error,hashlib,os
from pathlib import Path
R=Path(__file__).resolve().parent/'measurement';R.mkdir(exist_ok=True)
WORK=R/'workspace';WORK.mkdir(exist_ok=True)
# Bearer token and API base of a running AgentOS (codeg-server) instance.
TOKEN=os.environ['AGENTOS_TOKEN']
BASE=os.environ.get('AGENTOS_API','http://127.0.0.1:3080/api/')
OP=urllib.request.build_opener(urllib.request.ProxyHandler({}))
MODELS={'claude_code':'claude-haiku-5-5','codex':'gpt-6-luna','custom:devin':'swe-2-high'}
SEQ=['claude_code','codex','custom:devin','claude_code','custom:devin','codex','claude_code']
RESULT={'started_at':time.strftime('%Y-%m-%dT%H:%M:%S%z'),'planned':30,'scope':'Random settlement task continuation via real AgentOS ACP handoff; three agents, six directed edges, five rounds. Fresh conversations per round. No retry or exclusion of failed cases.','cases':[],'connections':[],'status':'running'}
J=(R/'journal.jsonl').open('a')
def log(k,**v):
 d={'time':time.time(),'kind':k,**v};J.write(json.dumps(d,ensure_ascii=False)+'\n');J.flush();print(k,json.dumps(v,ensure_ascii=False)[:450],flush=True)
def save():
 (R/'results.json').write_text(json.dumps(RESULT,ensure_ascii=False,indent=2)+'\n')
def api(n,**body):
 req=urllib.request.Request(BASE+n,data=json.dumps(body).encode(),headers={'Authorization':'Bearer '+TOKEN,'Content-Type':'application/json'})
 try:
  with OP.open(req,timeout=90) as r:return json.load(r)
 except urllib.error.HTTPError as e:raise RuntimeError(f'{n}: HTTP {e.code} '+e.read().decode()[:300])
def detail(conv):return api('get_folder_conversation',conversationId=conv)
def snap(c):return api('acp_get_session_snapshot',connectionId=c)
def allow_test_command(s,c):
 p=s['pending_permission'];raw=p.get('tool_call',{}).get('rawInput',{});cmd=raw.get('command','') or p.get('tool_call',{}).get('_meta',{}).get('cognition.ai/editableCommand','')
 expected="python3 -c 'import secrets,json; print(json.dumps(dict(token=secrets.token_hex(8),qty=secrets.randbelow(8)+1,unit_cents=secrets.randbelow(900)+101)))'"
 if cmd!=expected:raise RuntimeError('permission command differs from authorized test generator')
 options=p.get('options',[]);opt=next((o for o in options if o.get('kind')=='allow_once'),None)
 if opt is None:raise RuntimeError('no allow-once option')
 api('acp_respond_permission',connectionId=c,requestId=p['request_id'],optionId=opt['option_id']);log('authorized_test_generator',connection=c)
def wait_ready(c,timeout=90):
 end=time.monotonic()+timeout
 while time.monotonic()<end:
  s=snap(c)
  if s:
   status=str(s.get('status','')).lower()
   if s.get('pending_permission'):allow_test_command(s,c)
   if status in ('error','disconnected'):raise RuntimeError('connection state '+status)
   if status=='connected' and s.get('external_id'):return s
  time.sleep(1)
 raise TimeoutError('connection readiness')
def connect(agent,session=None):
 b={'agentType':agent,'workingDir':str(WORK),'preferredConfigValues':{'model':MODELS[agent]}}
 if session:b['sessionId']=session
 c=api('acp_connect',**b);RESULT['connections'].append(c);save();s=wait_ready(c)
 log('connected',agent=agent,connection=c,session=s.get('external_id'))
 return c

def send(conv,c,text):
 before=len(detail(conv).get('turns',[]))
 api('acp_prompt',connectionId=c,conversationId=conv,folderId=FOLDER,clientMessageId=str(uuid.uuid4()),blocks=[{'type':'text','text':text}])
 start=time.monotonic();last=None;stable=0
 while time.monotonic()-start<150:
  s=snap(c) or {};st=str(s.get('status','')).lower()
  if s.get('pending_permission'):allow_test_command(s,c)
  if st in ('error','disconnected'):raise RuntimeError('turn connection '+st)
  d=detail(conv);ts=d.get('turns',[])[before:]
  replies=[t for t in ts if t.get('role')=='assistant' and any(b.get('type')=='text' and b.get('text','').strip() for b in t.get('blocks',[]))]
  sig=json.dumps(ts,sort_keys=True)
  if st=='connected' and replies and not s.get('pending_user_message'):
   stable=stable+1 if sig==last else 0
   if stable>=2:return ts
  last=sig;time.sleep(1)
 raise TimeoutError('turn exceeded 150 seconds')
def block_text(t):return '\n'.join(b.get('text','') for b in t.get('blocks',[]) if b.get('type')=='text')
def objects(text):
 dec=json.JSONDecoder()
 for i,ch in enumerate(text):
  if ch=='{':
   try:
    d,_=dec.raw_decode(text[i:])
    if isinstance(d,dict):yield d
   except ValueError:pass

def run():
 global FOLDER
 FOLDER=api('open_folder',path=str(WORK))['id'];RESULT['folder_id']=FOLDER
 for rnd in range(1,6):
  conv=api('create_conversation',folderId=FOLDER,agentType=SEQ[0],title=f'任务接续成功率 R{rnd} {R.name}')
  RESULT.setdefault('conversations',[]).append(conv);save();current=None
  try:current=connect(SEQ[0])
  except Exception as e:log('initial_connect_failed',round=rnd,error=str(e))
  for idx in range(6):
   source,target=SEQ[idx:idx+2]
   case={'round':rnd,'edge':idx+1,'from':source,'to':target,'conversation':conv,'success':False};RESULT['cases'].append(case);save();log('case_start',round=rnd,edge=idx+1,source=source,target=target)
   try:
    if current is None:
     sm=detail(conv)['summary']
     if sm['agent_type']!=source:raise RuntimeError('conversation no longer matches scheduled source')
     current=connect(source,sm.get('external_id'))
    # The expected token and inputs exist only after actual tool execution.
    cmd="python3 -c 'import secrets,json; print(json.dumps(dict(token=secrets.token_hex(8),qty=secrets.randbelow(8)+1,unit_cents=secrets.randbelow(900)+101)))'"
    prompt='Settlement task. The rule is subtotal_cents = qty * unit_cents; shipping_cents = 175 if subtotal_cents < 3000 else 0; total_cents = subtotal_cents + shipping_cents. Run exactly this command to obtain this case\'s input: `'+cmd+'`. Do not write files. Your final reply must be exactly DONE; do not repeat the generated input or token. The next agent will finish the calculation.'
    prod=send(conv,current,prompt);case['producer_turns']=prod
    inputs=[]
    for t in prod:
     if t.get('role')!='assistant':continue
     for b in t.get('blocks',[]):
      if b.get('type')=='tool_result':
       inputs.extend(d for d in objects(b.get('output_preview','')) if {'token','qty','unit_cents'}<=d.keys())
    if len(inputs)!=1:raise RuntimeError(f'expected exactly one generated tool-result input; got {len(inputs)}')
    inp=inputs[0];case['input']=inp
    outside='\n'.join(block_text(t) for t in prod)
    if inp['token'] in outside:raise RuntimeError('producer repeated token in visible text; tool-result isolation failed')
    expected={'token':inp['token'],'subtotal_cents':inp['qty']*inp['unit_cents']}
    expected['shipping_cents']=175 if expected['subtotal_cents']<3000 else 0;expected['total_cents']=expected['subtotal_cents']+expected['shipping_cents'];case['expected']=expected
    sm=detail(conv)['summary'];start=time.monotonic()
    api('acp_switch_agent',conversationId=conv,expectedAgent=source,expectedSessionId=sm.get('external_id'),targetAgent=target)
    sm2=detail(conv)['summary'];target_conn=connect(target,sm2.get('external_id'));case['switch_seconds']=round(time.monotonic()-start,3)
    # Retire only our obsolete connection; does not touch other chats.
    try:api('acp_disconnect',connectionId=current)
    except Exception:pass
    current=target_conn
    recv=send(conv,current,'Continue the previous agent\'s settlement task. Use the latest generated tool-result input and the rule already in the conversation. Reply with exactly one JSON object with keys token, subtotal_cents, shipping_cents, total_cents. Do not call any tools or read any files.')
    case['receiver_turns']=recv
    answers='\n'.join(block_text(t) for t in recv if t.get('role')=='assistant');case['answer']=answers
    tool_used=any(b.get('type')=='tool_use' for t in recv if t.get('role')=='assistant' for b in t.get('blocks',[]))
    try:parsed=json.loads(answers.strip())
    except ValueError:parsed=None
    case['success']=parsed==expected and not tool_used
    if not case['success']:case['failure']='receiver used tools' if tool_used else 'receiver JSON did not exactly match expected result'
   except Exception as e:
    case['failure']=str(e);log('case_failed',round=rnd,edge=idx+1,error=str(e))
    # Re-align the scheduled target after a failed producer/transport, without retrying this case.
    if current:
     try:api('acp_cancel',connectionId=current)
     except Exception:pass
    try:
     sm=detail(conv)['summary']
     if sm['agent_type']!=target:
      api('acp_switch_agent',conversationId=conv,expectedAgent=sm['agent_type'],expectedSessionId=sm.get('external_id'),targetAgent=target)
     if current:
      try:api('acp_disconnect',connectionId=current)
      except Exception:pass
     sm=detail(conv)['summary'];current=connect(target,sm.get('external_id'))
    except Exception as ee:current=None;case['recovery_error']=str(ee)
   save();log('case_result',round=rnd,edge=idx+1,success=case['success'],passed=sum(c['success'] for c in RESULT['cases']),completed=len(RESULT['cases']))
  if current:
   try:api('acp_disconnect',connectionId=current)
   except Exception:pass
 RESULT['status']='completed';RESULT['passed']=sum(c['success'] for c in RESULT['cases']);RESULT['attempted']=len(RESULT['cases']);RESULT['success_rate']=RESULT['passed']/RESULT['attempted'];RESULT['ended_at']=time.strftime('%Y-%m-%dT%H:%M:%S%z');save()
try:run()
except Exception as e:RESULT['status']='interrupted';RESULT['error']=str(e);save();raise
finally:
 for c in RESULT['connections']:
  try:api('acp_disconnect',connectionId=c)
  except Exception:pass
 log('summary',status=RESULT['status'],passed=sum(c['success'] for c in RESULT['cases']),attempted=len(RESULT['cases']))
