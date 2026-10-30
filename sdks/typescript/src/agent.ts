//! ZhuyeAgent —— TS SDK 主类。
//!
//! 用法:
//! ```ts
//! const agent = await ZhuyeAgent.spawn();
//! await agent.registerTool('get_weather', '查询天气', {
//!   type: 'object', properties: { city: { type: 'string' } }, required: ['city']
//! }, async ({ city }) => ({ content: [{ type: 'text', text: `晴 26℃` }] }));
//! for await (const ev of agent.submit('北京天气如何')) {
//!   if (ev.msg.type === 'agent_message_delta') process.stdout.write(ev.msg.delta);
//! }
//! await agent.close();
//! ```

import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { Readable, Writable } from 'node:stream';

import { resolveBundledBinary } from './binary.js';
import {
  EVENT_ID_NONE,
  Event,
  EventMsg,
  Op,
  RemoteToolSpec,
  ReviewDecision,
  Submission,
  ToolOutput,
} from './protocol.js';

const ZHUYE_BIN_ENV = 'ZHUYE_BIN';
const DEFAULT_BIN = 'zhuye';
const READLINE_LIMIT = 16 * 1024 * 1024;
const STDIN_HANDSHAKE_TIMEOUT_MS = 30_000;
/** 握手失败时随异常携带的 stderr 尾部块数上限。 */
const STDERR_TAIL_LINES = 32;

/** 把 stderr 尾部 ring buffer 拼成诊断文本(解码容错)。 */
function stderrTailText(tail: Buffer[]): string {
  return tail
    .map((c) => c.toString('utf-8').trim())
    .filter((s) => s.length > 0)
    .join(' | ');
}

/** 单次工具执行的本地 handler(远程工具回执内容)。 */
export type ToolHandler = (
  args: Record<string, unknown>,
) => Promise<ToolOutput> | ToolOutput;

/** 已注册工具的内部记录。 */
interface RegisteredTool {
  spec: RemoteToolSpec;
  handler: ToolHandler;
}

/** ZhuyeAgent 启动选项。 */
export interface ZhuyeAgentOptions {
  /**
   * `zhuye` 二进制路径。缺省按优先级解析:env `ZHUYE_BIN` → 随包安装
   * 的平台子包内嵌二进制(`zhuye-<平台>` optional dependency)→
   * PATH 上的 `zhuye`。
   */
  bin?: string;
  /** 工作目录(传给 serve 的 env)。 */
  cwd?: string;
  /** 透传给 serve 进程的 env(如 `ZHUYE_MODEL=mock`)。 */
  env?: NodeJS.ProcessEnv;
  /** 反射 spawn options(覆盖/补充默认的 cwd / env)。 */
  spawnOptions?: Omit<SpawnOptions, 'bin' | 'args' | 'env' | 'cwd'>;
  /** `session_configured` 握手超时(ms)。默认 30s。 */
  handshakeTimeoutMs?: number;
  /**
   * 子进程 stderr 接管回调。提供后每个原始块交给回调(落文件 /
   * 送日志系统 / 过滤降噪),不再硬编码直通宿主 stderr。
   * 未提供时维持旧行为(透传宿主 stderr)。
   */
  onStderr?: (chunk: Buffer) => void;
}

interface SpawnOptions {
  bin: string;
  args: string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
}

/** serve 子进程的句柄(spawn 后管理 stdout 行 / 待处理 future)。 */
interface ServeProcess {
  child: ChildProcessWithoutNullStreams;
  stdin: Writable;
  stdoutLine: AsyncIterable<string>;
}

/**
 * 主类:启动并维护一个常驻 `zhuye serve` 子进程,把 JSONL 协议包装
 * 成 Promise / AsyncIterable 给 TS 用户。
 */
export class ZhuyeAgent {
  private readonly proc: ServeProcess;
  private readonly tools = new Map<string, RegisteredTool>();
  private readonly pending = new Map<string, (output: ToolOutput) => void>();
  private readonly ready: Promise<void>;
  private readonly _closeListeners: Array<(err?: Error) => void> = [];
  private closesHandled = false;
  private closed = false;
  private onceSessionConfigured: () => void = () => {};

  private constructor(proc: ServeProcess, ready: Promise<void>) {
    this.proc = proc;
    this.ready = ready;
    // 后台消费 stdout:按 type 分发到等待中的本地 handler(若已注册)
    // 或作为 turn 事件交给 submit() 的消费者。
    this.consumeStdout().catch((err) => {
      if (!this.closed) throw err;
    });
  }

  /** 启动一个 `zhuye serve` 子进程,等到 `session_configured` 即视为就绪。 */
  static async spawn(options: ZhuyeAgentOptions = {}): Promise<ZhuyeAgent> {
    // env 合并而非替换:保留 HOME / PATH 等(替换会让 zhuye 连
    // config 目录都定位不到),options.env 只做增量覆盖。
    const env: NodeJS.ProcessEnv = { ...process.env, ...options.env };
    const spawnOpts: SpawnOptions = {
      // 解析优先级:显式指定 → 环境变量 → 随平台子包内嵌的二进制 →
      // PATH 上的 `zhuye`。内嵌优先于 PATH:它与 SDK 版本严格配套。
      bin:
        options.bin ??
        process.env[ZHUYE_BIN_ENV] ??
        resolveBundledBinary() ??
        DEFAULT_BIN,
      args: ['serve'],
      cwd: options.cwd,
      env,
    };
    const child = spawn(spawnOpts.bin, spawnOpts.args, {
      cwd: spawnOpts.cwd,
      env: spawnOpts.env,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    // 子进程 stderr → 提供 onStderr 回调时交给回调;否则透传
    // 当前进程 stderr。无论哪种模式,尾部都保留在 ring buffer 里,
    // 握手期子进程死亡时把真实原因带进异常。
    const stderrTail: Buffer[] = [];
    child.stderr.on('data', (chunk: Buffer) => {
      stderrTail.push(chunk);
      if (stderrTail.length > STDERR_TAIL_LINES) stderrTail.shift();
      if (options.onStderr) {
        options.onStderr(chunk);
      } else {
        process.stderr.write(chunk);
      }
    });
    const proc: ServeProcess = {
      child,
      stdin: child.stdin,
      stdoutLine: lineStream(child.stdout),
    };
    // ready 在构造前组装:构造期 handshake 同步需要,放外面。
    let readyResolve!: () => void;
    let readyReject!: (err: Error) => void;
    const ready = new Promise<void>((res, rej) => {
      readyResolve = res;
      readyReject = rej;
    });
    // 握手期死亡时 ready 被拒绝;若调用方从未 await 它(spawn 后
    // 直接 close / 只用全局事件),挂一个 no-op 兜底标记已处理,
    // 避免 unhandled rejection 打爆宿主。后续 await 仍能拿到该错误。
    ready.catch(() => {});
    const agent = new ZhuyeAgent(proc, ready);
    // 握手是否仍在进行 —— 子进程在握手期退出时,ready 立刻拒绝
    // (带 stderr 尾部),不再傻等满握手超时后报泛化的 timed out。
    let handshakeSettled = false;
    // 子进程意外退出 → 通知所有 close 监听者(submit / prompt 等
    // 仍 await 中的 promise 借此抛错 / 收尾)。
    child.on('exit', (code, signal) => {
      // 阶段标记必须在 exit 时刻捕获:等待 stderr 排空期间握手状态
      // 可能被翻转,措辞判定要以死亡时的阶段为准。
      const wasHandshake = !handshakeSettled;
      // 握手期死亡与运行期死亡措辞区分,方便排障定位。
      const buildErr = () => {
        const tail = stderrTail.length
          ? ` stderr tail: ${stderrTailText(stderrTail)}`
          : '';
        return wasHandshake
          ? new Error(
              `zhuye serve exited during handshake (code=${code} signal=${signal ?? 'none'})${tail}`,
            )
          : new Error(
              `zhuye serve exited unexpectedly (code=${code} signal=${signal ?? 'none'})${tail}`,
            );
      };
      if (!wasHandshake) {
        agent.handleServeExit(buildErr());
        return;
      }
      // 竞态:子进程写 stderr 后立刻退出时,`exit` 事件可能先于
      // stderr 的 `data` 事件投递(管道排空晚于进程退出),此刻同步读
      // stderrTail 会拿到空尾、丢失真实原因。握手失败不在毫秒级关键
      // 路径上 —— 等 `close`(stdio 全部关闭,stderr 必已投递)或至多
      // 500ms 兜底,再构建错误。
      let drainSettled = false;
      const finish = () => {
        if (drainSettled) return;
        drainSettled = true;
        // 等待期间 stdout 缓冲可能投递了 session_configured:握手实际
        // 成功则让路,不再以失败拒绝。
        if (handshakeSettled) return;
        handshakeSettled = true;
        const err = buildErr();
        readyReject(err);
        agent.handleServeExit(err);
      };
      child.once('close', finish);
      const drainTimer = setTimeout(finish, 500);
      drainTimer.unref?.();
    });
    // spawn 失败(典型:二进制路径不存在,ENOENT):ChildProcess 的
    // error 事件没有监听者会以 uncaught exception 打爆宿主进程;这里
    // 转为握手失败(ready 尚未 settle 时立即拒绝,否则等握手超时)。
    child.on('error', (err) => {
      const failure = new Error(`zhuye serve failed to start: ${err.message}`);
      if (!handshakeSettled) {
        handshakeSettled = true;
        readyReject(failure);
      }
      agent.handleServeExit(failure);
    });
    const handshakeMs = options.handshakeTimeoutMs ?? STDIN_HANDSHAKE_TIMEOUT_MS;
    const timer = setTimeout(() => {
      if (handshakeSettled) return;
      handshakeSettled = true;
      readyReject(
        new Error(
          `zhuye serve handshake timed out after ${handshakeMs}ms` +
            (stderrTail.length
              ? ` stderr tail: ${stderrTailText(stderrTail)}`
              : ''),
        ),
      );
    }, handshakeMs);
    timer.unref?.();
    // 在握手阶段消费 stdout,直到见到 session_configured 后再切给
    // 业务消费者 —— 简单做法:让后台 consumer 自己处理,ready 在
    // session_configured 时 resolve。
    agent.onceSessionConfigured = () => {
      handshakeSettled = true;
      clearTimeout(timer);
      readyResolve();
    };
    return agent;
  }

  /** 测试 / mock 注入用:用现成的 stdio 流替代 spawn(单测走这条路)。 */
  static async fromStdio(
    stdin: Writable,
    stdout: Readable,
    handshakeTimeoutMs = STDIN_HANDSHAKE_TIMEOUT_MS,
  ): Promise<ZhuyeAgent> {
    const proc: ServeProcess = {
      // child 字段仅类型占位;内部不会触发 spawn 路径。
      child: undefined as unknown as ChildProcessWithoutNullStreams,
      stdin,
      stdoutLine: lineStream(stdout),
    };
    let readyResolve!: () => void;
    let readyReject!: (err: Error) => void;
    const ready = new Promise<void>((res, rej) => {
      readyResolve = res;
      readyReject = rej;
    });
    // 与 spawn 同款兜底:握手超时拒绝而调用方从未 await 时,
    // 不让 unhandled rejection 打爆宿主;后续 await 仍能拿到该错误。
    ready.catch(() => {});
    const agent = new ZhuyeAgent(proc, ready);
    const timer = setTimeout(() => {
      readyReject(new Error(`fromStdio handshake timed out after ${handshakeTimeoutMs}ms`));
    }, handshakeTimeoutMs);
    timer.unref?.();
    agent.onceSessionConfigured = () => {
      clearTimeout(timer);
      readyResolve();
    };
    return agent;
  }

  /** 注册客户端自定义工具(`ZhuyeAgent` 启动后任何时刻可调用)。 */
  async registerTool(
    name: string,
    description: string,
    parameters: Record<string, unknown>,
    handler: ToolHandler,
  ): Promise<void> {
    await this.ready;
    const spec: RemoteToolSpec = { name, description, parameters };
    this.tools.set(name, { spec, handler });
    const sub: Submission = {
      id: randomUUID(),
      op: { type: 'register_tools', tools: [spec] },
    };
    await this.writeSubmission(sub);
  }

  /** 提交一条用户 prompt;返回该 submission 的事件 AsyncIterable。 */
  async submit(text: string): Promise<AsyncIterable<Event>>;
  /** 提交一条任意 Op(用于 shutdown / interrupt / approval / 等等)。 */
  async submit(op: Op, opts?: { submissionId?: string }): Promise<AsyncIterable<Event>>;
  async submit(
    input: string | Op,
    opts: { submissionId?: string } = {},
  ): Promise<AsyncIterable<Event>> {
    await this.ready;
    const id = opts.submissionId ?? randomUUID();
    const op: Op = typeof input === 'string'
      ? {
          type: 'user_input',
          items: [{ type: 'text', text: input }],
        }
      : input;
    const sub: Submission = { id, op };
    await this.writeSubmission(sub);
    return this.filterEventsById(id);
  }

  /** 便捷:`prompt(text)` → 最终 assistant 文本(聚合所有 delta)。 */
  async prompt(text: string, opts: { timeoutMs?: number } = {}): Promise<string> {
    let result = '';
    for await (const ev of await this.submit(text)) {
      const msg = ev.msg;
      if (msg.type === 'agent_message_delta') result += msg.delta;
      else if (msg.type === 'agent_message') {
        // 全量消息是权威快照 —— 覆盖(而非追加)已累计的 delta,
        // 协议允许同轮混合下发时不会产生重复文本。text 经 typeof 守卫:
        // EventMsg 联合含开放成员(UnknownEventMsg),窄化后仍是 unknown。
        const { text } = msg as { text?: unknown };
        if (typeof text === 'string') result = text;
      }
      else if (msg.type === 'turn_complete') break;
      else if (msg.type === 'turn_aborted') {
        // 对齐 Python SDK:turn_aborted 不是正常完成,返回部分文本
        // 容易被调用方误认为完整答案 —— 抛错强制处理。
        throw new Error(`zhuye turn aborted: ${JSON.stringify(msg.reason)}`);
      }
      else if (msg.type === 'error') {
        throw new Error(`zhuye error: ${msg.code}: ${msg.message}`);
      }
    }
    return result;
  }

  /** 中断当前 turn。 */
  async interrupt(childId?: string): Promise<void> {
    await this.writeSubmission({
      id: randomUUID(),
      op: { type: 'interrupt', child_id: childId ?? null },
    });
  }

  /**
   * 响应 `ApprovalRequest` 或 `HookApprovalRequest`。
   *
   * `decision` 是 wire 形态的 `ReviewDecision`(serde snake_case):
   * `"approve"` / `"approve_for_session"`,或
   * `{ deny: { reason: string } }`。与 Python SDK 一致 —— 透传 wire
   * 值,不做形状转换(旧版发 `{type:'approve'}` 无法被服务端反序列化,
   * 审批会永久挂起,已修)。
   */
  async approve(id: string, decision: ReviewDecision): Promise<void> {
    await this.writeSubmission({
      id: randomUUID(),
      op: {
        type: 'tool_approval', // Hook 审批也用同一个 decision 形态;serve 层自行分发
        id,
        decision,
      },
    });
  }

  /**
   * 读取一条全局事件(`id=""`:approval_request / ask_user_question /
   * token_count / permission_mode_changed 等,含握手的
   * `session_configured` 本身),与 submit() 的事件迭代器相互独立;
   * 未知事件原样返回。
   *
   * stdout EOF / close 之后返回 `null`;`timeoutMs` 到点抛错 ——
   * 与 Python SDK 的 `next_global_event` 语义对齐(超时抛 `queue.Empty`,
   * EOF 返回 None)。
   */
  async nextGlobalEvent(opts: { timeoutMs?: number } = {}): Promise<Event | null> {
    const queued = this.globalQueue.shift();
    if (queued) return queued;
    if (this.globalEof) return null;
    return new Promise<Event | null>((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;
      const waiter = (ev: Event | null) => {
        if (timer !== undefined) clearTimeout(timer);
        resolve(ev);
      };
      this.globalWaiters.push(waiter);
      if (opts.timeoutMs !== undefined) {
        timer = setTimeout(() => {
          const idx = this.globalWaiters.indexOf(waiter);
          if (idx >= 0) this.globalWaiters.splice(idx, 1);
          reject(new Error(`zhuye nextGlobalEvent timed out after ${opts.timeoutMs}ms`));
        }, opts.timeoutMs);
        timer.unref?.();
      }
    });
  }

  /** 优雅关闭:发 Shutdown → 等 ShutdownComplete → 杀进程。 */
  async close(): Promise<void> {
    if (this.closed) return;
    // 先置 closed(同步挡住并发 close 与后续 writeSubmission),再发
    // shutdown。共享 stdin 可能已被别处(另一个 agent / 外部)end 掉,
    // 此时跳过写入与 end,直接走进程收尾。
    this.closed = true;
    this.closesHandled = true;
    const stdinUsable = !this.proc.stdin.destroyed && !this.proc.stdin.writableEnded;
    if (stdinUsable) {
      try {
        await this.writeSubmissionUnchecked({ id: randomUUID(), op: { type: 'shutdown' } });
      } catch {
        // stdin 已关,忽略
      }
      this.proc.stdin.end();
    }
    await new Promise<void>((resolve) => {
      if (!this.proc.child) {
        resolve();
        return;
      }
      this.proc.child.once('exit', () => resolve());
      setTimeout(() => {
        try {
          this.proc.child?.kill();
        } catch {
          // ignore
        }
        resolve();
        // 2s 对齐 Python SDK(agent.close 的 wait(timeout=2.0))。
      }, 2000).unref?.();
    });
  }

  /** 暴露当前进程句柄(测试需要直接操作 stdin / kill 时用)。 */
  get childProcess(): ChildProcessWithoutNullStreams | undefined {
    return this.proc.child;
  }

  /** 子进程意外退出时通知所有 close 监听者。 */
  private handleServeExit(err: Error): void {
    if (this.closesHandled) return;
    for (const cb of this._closeListeners) cb(err);
  }

  // ── 内部:握手 / 事件分发 / 写 Submission ─────────────────────────

  private async consumeStdout(): Promise<void> {
    for await (const line of this.proc.stdoutLine) {
      let ev: Event;
      try {
        ev = JSON.parse(line) as Event;
      } catch {
        process.stderr.write(`[zhuye-sdk] unparseable line: ${line.slice(0, 200)}\n`);
        continue;
      }
      // 握手阶段:见 session_configured 就 resolve ready。
      if (!this.onceSessionConfigured && ev.msg.type === 'session_configured') {
        // onceSessionConfigured 已被 fromStdio/spawn 重新赋值,这里只是兜底。
      }
      if (ev.msg.type === 'session_configured') {
        const cb = this.onceSessionConfigured;
        this.onceSessionConfigured = () => {};
        cb();
      }
      // 远程工具请求 → 调用本地 handler → 回执。
      if (ev.msg.type === 'tool_execution_request') {
        const req = ev.msg as {
          type: 'tool_execution_request';
          call_id: string;
          tool: string;
          args: Record<string, unknown>;
        };
        void this.handleToolRequest(req.call_id, req.tool, req.args);
        continue;
      }
      // 其他事件交给按 submission id 过滤的消费者。
      this.dispatchEvent(ev);
    }
    // stdout EOF(子进程退出):唤醒所有仍挂起的 submission 迭代器
    // 与全局事件等待者 —— 对齐 Python `_on_eof`:连接断开即按流末尾
    // 收尾,而不是永久挂起。
    for (const cb of [...this.eventListeners.values()]) cb(null);
    this.eventListeners.clear();
    this.pushGlobalEvent(null);
  }

  private async handleToolRequest(
    callId: string,
    tool: string,
    args: Record<string, unknown>,
  ): Promise<void> {
    const entry = this.tools.get(tool);
    if (!entry) {
      // 没注册 → 回执失败(让 LLM 看到错误)。
      await this.writeSubmission({
        id: randomUUID(),
        op: {
          type: 'tool_execution_response',
          call_id: callId,
          output: errorOutput(`tool '${tool}' not registered in this SDK`),
        },
      });
      return;
    }
    try {
      const output = await entry.handler(args);
      await this.writeSubmission({
        id: randomUUID(),
        op: { type: 'tool_execution_response', call_id: callId, output },
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      await this.writeSubmission({
        id: randomUUID(),
        op: {
          type: 'tool_execution_response',
          call_id: callId,
          output: errorOutput(message),
        },
      });
    }
  }

  private eventListeners = new Map<string, (ev: Event | null) => void>();
  /** 全局事件(`id=""`)缓冲:nextGlobalEvent() 的队列 + 挂起等待者。 */
  private globalQueue: Event[] = [];
  private globalWaiters: Array<(ev: Event | null) => void> = [];
  private globalEof = false;

  private dispatchEvent(ev: Event): void {
    if (ev.id) {
      const cb = this.eventListeners.get(ev.id);
      if (cb) {
        cb(ev);
        // 终态事件(turn_complete / turn_aborted / shutdown_complete /
        // submission_closed)视为 stream 末尾,清掉监听者。
        if (isTerminal(ev.msg)) this.eventListeners.delete(ev.id);
      }
    } else {
      // 全局事件(含握手的 session_configured)入会话级队列,由
      // nextGlobalEvent() 消费 —— 不入任何 submission 的迭代器。
      this.pushGlobalEvent(ev);
    }
  }

  /** 全局事件入队;`ev === null` 表示 EOF:标记收尾并唤醒等待者。 */
  private pushGlobalEvent(ev: Event | null): void {
    if (ev === null) this.globalEof = true;
    const waiter = this.globalWaiters.shift();
    if (waiter) {
      waiter(ev);
      return;
    }
    if (ev !== null) this.globalQueue.push(ev);
  }

  private filterEventsById(submissionId: string): AsyncIterable<Event> {
    const queue: Event[] = [];
    const pending: ((ev: Event | null) => void)[] = [];
    let closed = false;
    // ev === null 表示 stdout EOF / 连接断开:按流末尾收尾(效果与
    // 收到终态事件一致),对齐 Python `_on_eof` 唤醒全部监听者。
    const onEvent = (ev: Event | null) => {
      if (ev === null) {
        cleanup();
        return;
      }
      const next = pending.shift();
      if (next) next(ev);
      else queue.push(ev);
    };
    this.eventListeners.set(submissionId, onEvent);
    const cleanup = () => {
      closed = true;
      this.eventListeners.delete(submissionId);
      // 唤醒剩余 pending 等待者,让他们抛错或退出。
      while (pending.length) {
        const next = pending.shift();
        next?.(null);
      }
    };
    return {
      [Symbol.asyncIterator](): AsyncIterator<Event> {
        return {
          next: async (): Promise<IteratorResult<Event>> => {
            if (queue.length > 0) {
              const ev = queue.shift()!;
              if (isTerminal(ev.msg)) cleanup();
              return { value: ev, done: false };
            }
            if (closed) return { value: undefined, done: true };
            return new Promise((resolve) => {
              pending.push((ev) => {
                if (ev === null) resolve({ value: undefined, done: true });
                else {
                  if (isTerminal(ev.msg)) cleanup();
                  resolve({ value: ev, done: false });
                }
              });
            });
          },
          return: async () => {
            cleanup();
            return { value: undefined, done: true };
          },
        };
      },
    };
  }

  private async writeSubmission(sub: Submission): Promise<void> {
    // close() 之后(end 掉 stdin)仍可能有异步路径想写 —— 典型是挂起的
    // 工具 handler 回执。静默丢弃:连接已关,回执无处可去;抛"write
    // after end"反而变成 uncaught exception 打爆宿主进程。
    if (this.closed || this.proc.stdin.destroyed || this.proc.stdin.writableEnded) {
      return;
    }
    return this.writeSubmissionUnchecked(sub);
  }

  private writeSubmissionUnchecked(sub: Submission): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const payload = JSON.stringify(sub) + '\n';
      this.proc.stdin.write(payload, (err) => {
        if (err) reject(err);
        else resolve();
      });
    });
  }
}

function errorOutput(message: string): ToolOutput {
  return {
    content: [{ type: 'text', text: `error: ${message}` }],
    is_error: true,
    metadata: {},
    elapsed_ms: 0,
  };
}

function isTerminal(msg: EventMsg): boolean {
  // `submission_closed` 是 serve 在某条 submission 的 per-turn 通道排空后
  // 发出的收尾标记(挂该 submission id)。非 turn 操作(compact / rewind /
  // set_permission_mode 等)没有 turn_complete 之类终态事件,迭代器靠它
  // 收尾;turn 类操作先被 turn_complete / turn_aborted 终结,本事件届时
  // 已无监听者,被无害忽略。
  return (
    msg.type === 'turn_complete' ||
    msg.type === 'turn_aborted' ||
    msg.type === 'shutdown_complete' ||
    msg.type === 'submission_closed'
  );
}

/**
 * 把 Readable 流切成按行产生的 AsyncIterable。超长行(> 16 MiB)
 * 抛错并跳过 —— 对应 `zhuye serve` 不应输出如此长的 JSONL 行。
 */
function lineStream(stream: Readable): AsyncIterable<string> {
  // Node 的 Readable 默认 paused,data 事件不会自动 emit。.resume() 切
  // 到 flowing 模式,iter.next() 才有数据。
  stream.resume();
  const iter = stream[Symbol.asyncIterator]();
  let buffer = '';
  const MAX_LINE = READLINE_LIMIT;
  return {
    [Symbol.asyncIterator](): AsyncIterator<string> {
      return {
        next: async (): Promise<IteratorResult<string>> => {
          while (true) {
            const nl = buffer.indexOf('\n');
            if (nl >= 0) {
              const line = buffer.slice(0, nl);
              buffer = buffer.slice(nl + 1);
              return { value: line, done: false };
            }
            if (buffer.length > MAX_LINE) {
              const over = buffer.slice(0, 256);
              buffer = '';
              process.stderr.write(`[zhuye-sdk] line > ${MAX_LINE} bytes, dropping: ${over}\n`);
              continue;
            }
            const { value, done } = await iter.next();
            if (done) {
              if (buffer.length === 0) return { value: undefined, done: true };
              const tail = buffer;
              buffer = '';
              return { value: tail, done: false };
            }
            buffer += String(value);
          }
        },
        return: async () => {
          await iter.return?.();
          buffer = '';
          return { value: undefined, done: true };
        },
      };
    },
  };
}