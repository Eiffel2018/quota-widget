#!/usr/bin/env python3
"""Create synthetic-only Task-tree JSONL in an explicitly selected directory. No real accounts are read."""
import os,json,time,sys
from pathlib import Path
root=Path(sys.argv[1] if len(sys.argv)>1 else './synthetic-tree').resolve()
sessions=root/'sessions';sessions.mkdir(parents=True,exist_ok=True)
state=root/'state';state.mkdir(exist_ok=True)
now=int(time.time()); ids=[f'00000000-0000-4000-8000-{i:012d}' for i in range(1,16)]
names=['修復小工具','整理測試','解析歷史','驗證效能','改善介面','核對價格','測試日期','匯出資料','檢查重設','持久化測試','核對Token','驗證快取','檢查索引','測試來源','驗證視窗']
parents={**{i:0 for i in range(2,12)},12:2,13:1,14:1}
for i,(id,name) in enumerate(zip(ids,names)):
 rows=[];meta={'id':id,'thread_name':name}
 if i in parents:meta['source']={'subagent':{'thread_spawn':{'parent_thread_id':ids[parents[i]],'depth':2 if i==12 else 1}}}
 rows.append({'type':'session_meta','timestamp':now-700,'payload':meta})
 if i==0:
  for dt,used in [(-600,97),(-500,98),(-499,97),(-497,98),(-490,97),(-480,98)]:
   rows.append({'type':'event_msg','timestamp':now+dt,'payload':{'type':'token_count','thread_id':id,'rate_limits':{'primary':{'used_percent':used,'window_minutes':10080,'resets_at':now+500000}}}})
 rows.append({'type':'turn_context','timestamp':now-450+i,'payload':{'model':'gpt-6.1-sol','turn_id':f'turn-{i}'}})
 rows.append({'type':'token_usage_record','timestamp':now-200+i,'payload':{'thread_id':id,'turn_id':f'turn-{i}','response_id':f'response-{i}','usage':{'input_tokens':132534,'cached_input_tokens':132096,'output_tokens':95,'reasoning_output_tokens':74},'turn_token_usage':{'input_tokens':38788508,'cached_input_tokens':38000000,'output_tokens':60924,'reasoning_output_tokens':10000}}})
 (sessions/f'{i:02}.jsonl').write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in rows))
(root/'session_index.jsonl').write_text(''.join(json.dumps({'id':id,'thread_name':name,'updated_at':now},ensure_ascii=False)+'\n' for id,name in zip(ids,names)))
settings={'sessions_path':str(sessions),'import_enabled':True,'on_top':True,'dark':True,'task_visible_rows':10,'size':[310,360],'bucket':'codex','window':'primary'}
(state/'settings.json').write_text(json.dumps(settings))
(root/'fixture-metadata.json').write_text(json.dumps({'now':now,'ids':ids,'parents':parents},indent=2))
print(root)

print('Run the widget with --offline --data-dir',state)
