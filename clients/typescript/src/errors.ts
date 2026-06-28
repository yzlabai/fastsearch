// 错误类型：把 HTTP 状态码暴露出来，便于 agent 侧按 401/429 做分流（重认证 / 退避重试）。

/** fastsearch 协议/HTTP 错误。`status` 为 HTTP 状态码（网络层错误为 0）。 */
export class FastsearchError extends Error {
  /** HTTP 状态码；网络/超时等传输层错误为 0。 */
  readonly status: number;
  /** 服务端返回的原始错误体（若有）。 */
  readonly detail: string;

  constructor(message: string, status = 0, detail = "") {
    super(message);
    this.name = "FastsearchError";
    this.status = status;
    this.detail = detail;
  }

  /** 认证失败（401）：API Key 缺失/无效。 */
  get isAuth(): boolean {
    return this.status === 401;
  }

  /** 被限流（429）：应退避后重试。 */
  get isRateLimited(): boolean {
    return this.status === 429;
  }

  /** 可重试：限流或 5xx 或传输层错误。 */
  get isRetryable(): boolean {
    return this.status === 0 || this.status === 429 || this.status >= 500;
  }
}
