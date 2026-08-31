# fastsearch 检索与摄取上下文

fastsearch 把可引用文档切片作为检索真源，并以摄取作业描述原始文档变成切片的过程。

## Language

**文档坐标（Document Coordinate）**：
由 `(collection, doc_id)` 构成的系统级全局文档标识；一个坐标只能属于一个 tenant。
_Avoid_: tenant 内文档 ID、局部 doc_id

**全局切片 ID（GlobalId）**：
由 `(collection, doc_id, chunk_id)` 构成、可直接进入 citation 的全局切片标识，不携带 tenant。
_Avoid_: tenant-scoped chunk ID

**Tenant**：
文档的可见性与所有权边界；它不是文档坐标的一部分，不能让相同坐标出现第二份真源。
_Avoid_: namespace、document key

**作业租约（Job Lease）**：
绑定 `job_id + lease_owner + lease_epoch` 的一次 worker 处理权；三者任一不匹配都不是当前租约。
_Avoid_: epoch、worker key
