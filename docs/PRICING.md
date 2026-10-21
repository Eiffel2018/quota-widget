# API-equivalent pricing assumptions

Verified 2026-10-17 against official OpenAI pages:
- https://developers.openai.com/api/docs/pricing
- https://developers.openai.com/api/docs/models/gpt-6.1-sol
- https://developers.openai.com/api/docs/models/gpt-6-luna

USD per 1 million tokens, Standard, input / cached input / output:
- gpt-6.1-sol: 2.00 / 0.10 / 10.00
- gpt-6-luna: 0.10 / 0.01 / 0.50

Fast / Priority preset is 2× each Standard rate. If total input for one known request exceeds 272,000 tokens, the entire request uses 2× input and cached-input rates and 1.5× output rate. The exact threshold uses `>` (272,000 itself stays standard-context). This applies to the chosen tier's rates. Reasoning is already within output and is not billed twice.

New GUI settings assume Standard. Existing custom rates are preserved. The log may not reveal reliable service tier; the UI states its global assumption and allows changes. Missing models assume gpt-6.1-sol, visibly marked. API-equivalent USD is not a subscription charge, credit conversion, or quota allocation. These are dated estimates, not a live tariff service.

Not modeled separately: cache-write premiums (a reported positive counter is flagged and its event cost remains unknown), regional surcharges, tool charges, Batch/Flex or Ultrafast. A multi-request cumulative gap cannot safely select a per-request long-context bracket; such events retain unknown cost. Unknown is never silently zero. Historical amounts are recalculated using the selected table rather than a claim to historical invoice reconciliation.
