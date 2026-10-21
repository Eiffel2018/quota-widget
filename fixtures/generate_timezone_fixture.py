#!/usr/bin/env python3
"""Synthetic fixed-UTC+8, effort and partial-subtotal UI fixture."""
import json,sys
from datetime import datetime
from pathlib import Path
root=Path(sys.argv[1]).resolve(); sessions=root/'sessions';sessions.mkdir(parents=True,exist_ok=True);state=root/'state';state.mkdir(exist_ok=True)
def ts(value):return int(datetime.fromisoformat(value.replace('Z','+00:00')).timestamp())
def counts(i=20000,c=4000,o=6000,r=2000):return {'input_tokens':i,'cached_input_tokens':c,'output_tokens':o,'reasoning_output_tokens':r}
def write(name,rows):(sessions/(name+'.jsonl')).write_text(''.join(json.dumps(row,ensure_ascii=False)+'\n' for row in rows))
def meta(id,name):return {'timestamp':ts('2026-10-17T15:00:00Z'),'type':'session_meta','payload':{'id':id,'thread_name':name}}
def context(id,model,effort,t):return {'timestamp':t-1,'type':'turn_context','payload':{'turn_id':id,'model':model,'effort':effort}}
def response(task,turn,id,t,usage):return {'timestamp':t,'type':'token_usage_record','payload':{'thread_id':task,'turn_id':turn,'response_id':id,'usage':usage,'turn_token_usage':usage}}
t=ts('2026-10-17T23:51:00Z')
write('partial',[meta('partial','Luna 部分已知小計'),context('turn-partial','gpt-6-luna','max',t-2),{'timestamp':t-1,'type':'event_msg','payload':{'type':'token_count','turn_id':'turn-partial','info':{'total_token_usage':counts(50000,10000,15000,5000),'last_token_usage':counts()}}},response('partial','turn-partial','partial-response',t,counts())])
write('normal',[meta('normal','Sol high'),context('turn-sol','gpt-6.1-sol','high',t+5),response('normal','turn-sol','sol-response',t+5,counts())])
missing=counts();missing.pop('reasoning_output_tokens')
write('missing',[meta('missing','未記錄強度／推理數'),context('turn-missing','gpt-6-luna',None,t+10),response('missing','turn-missing','missing-response',t+10,missing)])
write('prior',[meta('prior','香港前一日'),context('turn-prior','gpt-6-luna','low',ts('2026-10-17T15:59:59Z')),response('prior','turn-prior','prior-response',ts('2026-10-17T15:59:59Z'),counts())])
(state/'settings.json').write_text(json.dumps({'sessions_path':str(sessions),'import_enabled':True,'dark':True,'on_top':True,'size':[400,450],'task_visible_rows':10,'bucket':'codex','window':'primary'}))
(root/'expected.json').write_text(json.dumps({'utc_2351_epoch':t,'hk_display':'2026-10-18 07:51:00 +08:00','default_hk_date':'2026-10-18','daily_responses':3,'partial_known_usd':.00464,'effort':'max','reasoning_tokens':2000},indent=2))
print(root)
