# fastsearch-client (Python)

零依赖（仅标准库）的 fastsearch REST 客户端。

```python
from fastsearch_client import FastsearchClient

c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")

# 灌入 docparse chunks（docparse -f chunks 的输出）
import json, subprocess
chunks = json.loads(subprocess.check_output(["docparse", "report.pdf", "-f", "chunks"]))
c.index("kb", "report.pdf", chunks)

# 检索（带 page+bbox 引用溯源）
for h in c.search("kb", "毛利率为什么下降", top_k=10):
    print(h["citation_id"], "p", h["page"], h["bbox"])
```

ACL 由服务端按 API Key 强制，客户端无法越权。许可 Apache-2.0。
