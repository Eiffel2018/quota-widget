#!/usr/bin/env python3
"""Synthetic 286-descendant regression. Does not read any real user files."""
import json,sys,time
from pathlib import Path
root=Path(sys.argv[1]).resolve(); sessions=root/'sessions'; state=root/'state'
sessions.mkdir(parents=True,exist_ok=True);state.mkdir(exist_ok=True)
now=int(time.time()); main='synthetic-main'; parent={}
rows=[{'type':'session_meta','timestamp':now-100000,'payload':{'id':main,'thread_name':'合成主代理'}}]
for dt,used in [(-18001,40),(-18000,41)]:
 rows.append({'type':'event_msg','timestamp':now+dt,'payload':{'type':'token_count','thread_id':main,'rate_limits':{'primary':{'used_percent':used,'window_minutes':10080,'resets_at':now+500000}}}})
def usage(task,stamp,model='gpt-6.1-sol'):
 return [{'type':'turn_context','timestamp':stamp-1,'payload':{'model':model,'turn_id':'turn-'+task}},{'type':'token_usage_record','timestamp':stamp,'payload':{'thread_id':task,'turn_id':'turn-'+task,'response_id':'response-'+task,'usage':{'input_tokens':132534,'cached_input_tokens':132096,'output_tokens':95,'reasoning_output_tokens':74},'turn_token_usage':{'input_tokens':132534,'cached_input_tokens':132096,'output_tokens':95,'reasoning_output_tokens':74}}}]
rows+=usage(main,now-30)
(sessions/'main.jsonl').write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in rows))
children=[('recent-a','近期子代理A',main,now-60),('recent-b','近期子代理B',main,now-90),('old-contributor','本%較早用量',main,now-10000),('middle','必要上層',main,None),('leaf','近期孫代理','middle',now-100)]
children += [(f'stale-{i:03}','舊紀錄'+str(i),main,now-86400) for i in range(281)]
for task,name,p,stamp in children:
 parent[task]=p
 rs=[{'type':'session_meta','timestamp':now-100000,'payload':{'id':task,'thread_name':name,'source':{'subagent':{'thread_spawn':{'parent_thread_id':p,'depth':2 if p=='middle' else 1}}}}}]
 if stamp is not None:rs+=usage(task,stamp,'gpt-6-luna')
 (sessions/(task+'.jsonl')).write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in rs))
(state/'settings.json').write_text(json.dumps({'sessions_path':str(sessions),'import_enabled':True,'on_top':True,'dark':True,'task_visible_rows':10,'size':[430,570],'position':[100,80],'bucket':'codex','window':'primary'}))
(root/'fixture-metadata.json').write_text(json.dumps({'now':now,'recorded_descendants':286,'relevant_descendants':5,'stale_descendants':281},indent=2))
print(root)
