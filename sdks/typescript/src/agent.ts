//! ReflectAgent —— TS SDK 主类。
//!
//! 用法:
//! ```ts
//! const agent = await ReflectAgent.spawn();
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

const REFLECT_BIN_ENV = 'REFLECT_BIN';
const DEFAULT_BIN = 'reflect';
const READLINE_LIMIT = 16 * 1024 * 1024;
const STDIN_HANDSHAKE_TIMEOUT_MS = 30_000;

/** 单次工具执行的本地 handler(远程工具回执内容)。 */
export type ToolHandler = (
  args: Record<string, unknown>,
) => Promise<ToolOutput> | ToolOutput;

/** 已注册工具的内部记录。 */
interface RegisteredTool {
  spec: RemoteToolSpec;
  handler: ToolHandler;
}

/** ReflectAgent 启动选项。 */
export interface ReflectAgentOptions {
  /** `reflect` 二进制路径。默认读 env `REFLECT_BIN` → PATH 上的 `reflect`。 */
  bin?: string;
  /** 工作目录(传给 serve 的 env)。 */
  cwd?: string;
  /** 透传给 serve 进程的 env(如 `REFLECT_MODEL=mock`)。 */
  env?: NodeJS.ProcessEnv;
  /** 反射 spawn options(覆盖/补充默认的 cwd / env)。 */
  spawnOptions?: Omit<SpawnOptions, 'bin' | 'args' | 'env' | 'cwd'>;
  /** `session_configured` 握手超时(ms)。默认 30s。 */
  handshakeTimeoutMs?: number;
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
 * 主类:启动并维护一个常驻 `reflect serve` 子进程,把 JSONL 协议包装
 * 成 Promise / AsyncIterable 给 TS 用户。
 */
export class ReflectAgent {
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

  /** 启动一个 `reflect serve` 子进程,等到 `session_configured` 即视为就绪。 */
  static async spawn(options: ReflectAgentOptions = {}): Promise<ReflectAgent> {
    // env 合并而非替换:保留 HOME / PATH 等(替换会让 reflect 连
    // config 目录都定位不到),options.env 只做增量覆盖。
    const env: NodeJS.ProcessEnv = { ...process.env, ...options.env };
    const spawnOpts: SpawnOptions = {
      bin: options.bin ?? process.env[REFLECT_BIN_ENV] ?? DEFAULT_BIN,
      args: ['serve'],
      cwd: options.cwd,
      env,
    };
    const child = spawn(spawnOpts.bin, spawnOpts.args, {
      cwd: spawnOpts.cwd,
      env: spawnOpts.env,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    // 子进程 stderr → 当前进程 stderr(让 tracing 日志透传)。
    child.stderr.on('data', (chunk: Buffer) => {
      process.stderr.write(chunk);
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
    const agent = new ReflectAgent(proc, ready);
    // 子进程意外退出 → 通知所有 close 监听者(submit / prompt 等
    // 仍 await 中的 promise 借此抛错 / 收尾)。
    child.on('exit', (code, signal) => {
      agent.handleServeExit(
        new Error(
          `reflect serve exited unexpectedly (code=${code} signal=${signal ?? 'none'})`,
        ),
      );
    });
    const handshakeMs = options.handshakeTimeoutMs ?? STDIN_HANDSHAKE_TIMEOUT_MS;
    const timer = setTimeout(() => {
      readyReject(new Error(`reflect serve handshake timed out after ${handshakeMs}ms`));
    }, handshakeMs);
    timer.unref?.();
    // 在握手阶段消费 stdout,直到见到 session_configured 后再切给
    // 业务消费者 —— 简单做法:让后台 consumer 自己处理,ready 在
    // session_configured 时 resolve。
    agent.onceSessionConfigured = () => {
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
  ): Promise<ReflectAgent> {
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
    const agent = new ReflectAgent(proc, ready);
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

  /** 注册客户端自定义工具(`ReflectAgent` 启动后任何时刻可调用)。 */
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
      else if (msg.type === 'agent_message') result += msg.text;
      else if (msg.type === 'turn_complete') break;
      else if (msg.type === 'turn_aborted') break;
      else if (msg.type === 'error') {
        throw new Error(`reflect error: ${msg.code}: ${msg.message}`);
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
      }, 1000).unref?.();
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
        process.stderr.write(`[reflect-sdk] unparseable line: ${line.slice(0, 200)}\n`);
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

  private eventListeners = new Map<string, (ev: Event) => void>();
  private sessionListeners: Array<(ev: Event) => void> = [];

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
      // 全局事件(SessionConfigured 之后)转发给会话级订阅。
      for (const cb of this.sessionListeners) cb(ev);
    }
  }

  private filterEventsById(submissionId: string): AsyncIterable<Event> {
    const queue: Event[] = [];
    const pending: ((ev: Event | null) => void)[] = [];
    let closed = false;
    const onEvent = (ev: Event) => {
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
 * 抛错并跳过 —— 对应 `reflect serve` 不应输出如此长的 JSONL 行。
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
              process.stderr.write(`[reflect-sdk] line > ${MAX_LINE} bytes, dropping: ${over}\n`);
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