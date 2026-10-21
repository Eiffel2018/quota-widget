#!/usr/bin/env python3
"""Write synthetic-only review/model-pricing fixtures to an explicit directory."""
import json,sys,time
from pathlib import Path
root=Path(sys.argv[1] if len(sys.argv)>1 else './synthetic-review').resolve(); sessions=root/'sessions';sessions.mkdir(parents=True,exist_ok=True); state=root/'state';state.mkdir(exist_ok=True)
now=int(time.time()); names=[]; completed='--with-completed' in sys.argv
def write_task(id,name,model,ordinal,parent=None,extra=None):
 meta={'id':id,'thread_name':name}
 if parent:meta['parent_thread_id']=parent
 rows=[{'type':'session_meta','timestamp':now-1000,'payload':meta}]
 if id=='sol-parent':
  polls=[(dt,40 if dt < -780 else 41 if dt < -120 else 43) for dt in range(-960,1,60)] if completed else [(-900,40),(-800,41)]
  for dt,used in polls:rows.append({'timestamp':now+dt,'type':'event_msg','payload':{'type':'token_count','thread_id':id,'rate_limits':{'primary':{'used_percent':used,'window_minutes':10080,'resets_at':now+500000}}}})
 def response(model,turn,stamp,large=False):
  rows.append({'type':'turn_context','timestamp':stamp-1,'payload':{'model':model,'turn_id':turn,'reasoning_effort':'low' if large else ('max' if model=='gpt-6-luna' else 'high')}})
  usage={'input_tokens':50000000 if large else 100000,'cached_input_tokens':49000000 if large else 90000,'output_tokens':50000 if large else 1000,'reasoning_output_tokens':40000 if large else 600}
  rows.append({'type':'token_usage_record','timestamp':stamp,'payload':{'thread_id':id,'turn_id':turn,'response_id':turn+'-response','usage':usage,'turn_token_usage':usage}})
 if completed and id=='sol-parent':response(model,id+'-baseline',now-950)
 response(model,id+'-turn',now-500+ordinal,model=='codex-auto-review')
 if extra:response(extra,id+'-switch',now-200)
 (sessions/(id+'.jsonl')).write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in rows));names.append({'id':id,'thread_name':name,'updated_at':now})
write_task('sol-parent','主代理 Sol','gpt-6.1-sol',0)
for i in range(7):write_task(f'luna-child-{i}',f'Luna 子代理{i+1}','gpt-6-luna',i+1,'sol-parent')
for i in range(19):write_task(f'auto-review-{i}',f'Auto review {i+1}','codex-auto-review',i+8,'sol-parent')
write_task('ordinary-code-review','正常 Sol 程式審查','gpt-6.1-sol',28)
write_task('mixed-model-task','Sol→Luna 混合模型','gpt-6.1-sol',29,extra='gpt-6-luna')
(root/'session_index.jsonl').write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in names))
(state/'settings.json').write_text(json.dumps({'sessions_path':str(sessions),'import_enabled':True,'dark':True,'on_top':True,'size':[400,450],'task_visible_rows':10,'bucket':'codex','window':'primary'}))
(root/'expected.json').write_text(json.dumps({'included_events':11,'excluded_events':19,'visible_tasks':10,'parent_descendants':7,'luna_each_usd':0.0024,'parent_usd':0.039,'luna_children_usd':0.0168,'mixed_thread_usd':0.0414,'global_usd':0.1362},indent=2))
print(root)
