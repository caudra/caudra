import asyncio
results = await asyncio.gather(executor.search(query="", limit=5), executor.search(query="", namespace="executor", limit=5))
[{"items": r["items"], "namespaces": r["namespaces"], "unavailableApps": r["unavailableApps"]} for r in results]
