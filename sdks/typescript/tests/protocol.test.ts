//! ZhuyeAgent 协议回路测试:用一个轻量 mock serve 进程驱动协议。
//!
//! mock serve 由 Node 直接跑一个 stdin→stdout 回显脚本(模拟 zhuye
//! serve 的 JSONL 协议),无需 spawn `zhuye` 二进制,测试完全离线。
//!
//! 行为契约:
//! - 收到任何 stdin 一行 → emit `session_configured`(`id=""`)回应;
//! - 之后把每行 stdin 解析为 Submission,emit 一个匹配 submission.id
//!   的 turn_complete 事件 + agent_message_delta。
//! - 收到 `register_tools` op → 不发任何事件(注册被静默接受);
//! - 收到 `tool_execution_response` op → 不发任何事件(回执被静默接受);
//! - 收到 `shutdown` op → 发 shutdown_complete 后退出。

import { spawn, ChildProcessWithoutNullStreams } from 'node:child_process';
import { Readable, Writable, PassThrough } from 'node:stream';

import { describe, expect, it, beforeEach, afterEach, expectTypeOf } from 'vitest';

import {
  EVENT_ID_NONE,
  Event,
  EventMsg,
  ZhuyeAgent,
} from '../src/index.js';

import type { TurnComplete, AbortReason, PlanApprovalChoice, ReviewDecision, ToolCallOutputDelta } from '../src/index.js';

it('公开类型与 Rust wire 值一致', () => {
  expectTypeOf<TurnComplete['status']>().toEqualTypeOf<'success' | 'max_iterations' | 'stopped' | 'token_budget_exceeded'>();
  expectTypeOf<AbortReason>().toEqualTypeOf<{ type: 'user_interrupt' } | { type: 'shutdown' } | { type: 'error'; code: string; message: string }>();
  expectTypeOf<PlanApprovalChoice>().toEqualTypeOf<'auto_mode' | 'manual_approve' | 'revise'>();
  expectTypeOf<ReviewDecision>().toEqualTypeOf<'approve' | 'approve_for_session' | { deny: { reason: string } }>();
  expectTypeOf<ToolCallOutputDelta>().toEqualTypeOf<{ type: 'tool_call_output_delta'; call_id: string; delta: string; is_stderr: boolean }>();
});

/**
 * spawn 一个本地 Node "mock serve" 子进程:读 stdin JSONL、写 stdout JSONL。
 *
 * 把"收到 register_tools / tool_execution_response 不回事件"这一段
 * 行为作为契约,SDK 测试可以放心 assert:
 * - 写入 stdin 后收到对应 turn_complete;
 * - registerTools 后不会立刻被回执事件淹没;
 * - shutdown 后子进程优雅退出。
 */
/** spawn 一个本地 Node "mock serve" 子进程:读 stdin JSONL、写 stdout JSONL。
 *
 * 启动后**立即** emit `session_configured`(不依赖 stdin),便于 agent
 * 握手后再处理第一条 submission。后续收到 register_tools /
 * tool_execution_response 静默接受;user_input → emit turn 三件套;
 * shutdown → emit shutdown_complete 并 exit。
 */
function spawnMockServe(): ChildProcessWithoutNullStreams {
  const script = `
    process.stdout.write(JSON.stringify({
      id: '',
      msg: {
        type: 'session_configured',
        session_id: 'mock-session',
        model: 'mock/mock-1',
        provider: 'mock',
        approval_policy: 'auto',
        sandbox_policy: 'workspace_only',
      },
    }) + '\\n');
    const readline = require('node:readline');
    const rl = readline.createInterface({ input: process.stdin });
    rl.on('line', (line) => {
      let sub;
      try { sub = JSON.parse(line); } catch { return; }
      const opType = sub.op?.type;
      if (opType === 'shutdown') {
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'shutdown_complete' },
        }) + '\\n');
        process.exit(0);
        return;
      }
      if (opType === 'register_tools' || opType === 'tool_execution_response') {
        return;
      }
      if (opType === 'user_input' || opType === 'interrupt') {
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'turn_started', turn_id: 't', user_message_id: 'm' },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'agent_message_delta', delta: 'mock-reply' },
        }) + '\\n');
        // v1.5 additive 变体:工具参数增量(SDK 应原样透传给客户端)。
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: {
            type: 'tool_call_args_delta', call_id: 'c1', delta: '{"text":"pi',
          },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'tool_call_output_delta', call_id: 'c1', delta: 'preview', is_stderr: true },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: {
            type: 'turn_complete', turn_id: 't', status: 'success',
            usage: { input_tokens: 0, output_tokens: 1 },
          },
        }) + '\\n');
      }
    });
  `;
  const child = spawn(process.execPath, ['-e', script], { stdio: ['pipe', 'pipe', 'pipe'] });
  // 把 mock serve 的 stderr 重定向到当前 stderr(可观测,避免泄漏);
  // 实际脚本不写 stderr,这里只是兜底。
  child.stderr.on('data', (c: Buffer) => process.stderr.write(c));
  return child;
}

describe('ZhuyeAgent 协议回路(mock serve 驱动)', () => {
  let agent: ZhuyeAgent;
  let child: ChildProcessWithoutNullStreams;

  beforeEach(async () => {
    child = spawnMockServe();
    // 直接喂 stdin / stdout —— 等同于 ZhuyeAgent.spawn 但跳过 spawn。
    const stdin = child.stdin as Writable;
    const stdout = child.stdout as Readable;
    agent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
  }, 10_000);

  afterEach(async () => {
    try {
      await agent.close();
    } catch {
      // 忽略关闭阶段双工关错。
    }
    if (!child.killed) child.kill();
  });

  it('prompt 高层 API 聚合 delta 并返回', async () => {
    expect(await agent.prompt('hi')).toBe('mock-reply');
  }, 10_000);

  it('registerTools 静默接受(不污染 turn 事件流)', async () => {
    await agent.registerTool('foo', 'd', { type: 'object' }, () => ({
      content: [{ type: 'text', text: 'x' }],
      is_error: false,
      metadata: {},
      elapsed_ms: 0,
    }));
    expect(await agent.prompt('hi')).toBe('mock-reply');
  }, 10_000);

  it('shutdown 子命令序列化 + 子进程退出', async () => {
    const closeP = agent.close();
    // 等到子进程 exit / zhuye exit code == 0
    const exitPromise = new Promise<number | null>((resolve) => {
      child.once('exit', (code) => resolve(code));
    });
    await closeP;
    const code = await exitPromise;
    expect(code).toBe(0);
  }, 10_000);

  it('UnknownEventMsg 降级不抛错', async () => {
    // 每个 SDK 实例独占输入输出流,避免两个消费者争抢同一子进程的握手。
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const customAgent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
    try {
      stdout.write(JSON.stringify({ id: '', msg: {
        type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only',
      } }) + '\n');
      const stream = await customAgent.submit(
        { type: 'user_input', items: [{ type: 'text', text: 'probe' }] },
        { submissionId: 'unknown-probe' },
      );
      stdout.write(JSON.stringify({ id: 'unknown-probe', msg: { type: 'telemetry_event', payload: { x: 1 } } }) + '\n');
      stdout.write(JSON.stringify({ id: 'unknown-probe', msg: {
        type: 'turn_complete', turn_id: 't', status: 'success', usage: {},
      } }) + '\n');
      const events: Event[] = [];
      for await (const event of stream) events.push(event);
      expect(events[0].msg).toEqual({ type: 'telemetry_event', payload: { x: 1 } });
      expect(events.at(-1)?.msg.type).toBe('turn_complete');
    } finally {
      await customAgent.close();
      stdout.end();
    }
  }, 10_000);

  it('tool_call_args_delta 透传到 turn 事件流(additive 变体)', async () => {
    // 协议变更是 additive 也必须同步 SDK —— 客户端要能拿到工具参数增量
    // 做渐进渲染;字段名保持 snake_case,与 Rust serde 一致。
    const events: EventMsg[] = [];
    for await (const ev of await agent.submit('foo')) {
      events.push(ev.msg);
      if (ev.msg.type === 'turn_complete') break;
    }
    const d = events.find((e) => e.type === 'tool_call_args_delta');
    expect(d).toBeDefined();
    expect(d).toMatchObject({
      type: 'tool_call_args_delta',
      call_id: 'c1',
      delta: '{"text":"pi',
    });
  }, 10_000);

  it('工具输出增量字段透传', async () => {
    const events: EventMsg[] = [];
    for await (const ev of await agent.submit('foo')) events.push(ev.msg);
    expect(events.find((e) => e.type === 'tool_call_output_delta')).toEqual({
      type: 'tool_call_output_delta', call_id: 'c1', delta: 'preview', is_stderr: true,
    });
    expect(events.at(-1)?.type).toBe('turn_complete');
  }, 10_000);

  it('未知事件变体降级不崩', async () => {
    // 协议新增变体时,SDK 不得因不认识而抛错(按未知 type 降级)。
    (child.stdout as Readable).push(
      JSON.stringify({ id: '', msg: { type: 'some_future_event', payload: { x: 1 } } }) + '\n',
    );
    // 不抛即通过;再确认 agent 仍可用。
    expect(await agent.prompt('foo')).toBe('mock-reply');
  }, 10_000);

  it('迭代所有 turn 事件类型后正确收尾', async () => {
    const events: EventMsg[] = [];
    for await (const ev of await agent.submit('foo')) {
      events.push(ev.msg);
      if (ev.msg.type === 'turn_complete') break;
    }
    const types = events.map((e) => e.type);
    expect(types).toContain('turn_started');
    expect(types).toContain('agent_message_delta');
    expect(types[types.length - 1]).toBe('turn_complete');
  }, 10_000);

  it('nextGlobalEvent 读取全局事件(含握手事件本身)', async () => {
    // 握手 session_configured(id="")也进入全局队列(对齐 Python
    // `_global_queue` 语义:K9 e2e 依赖此行为)。
    const handshake = await agent.nextGlobalEvent({ timeoutMs: 2000 });
    expect(handshake?.id).toBe('');
    expect(handshake?.msg.type).toBe('session_configured');
    // 注入一条后续全局事件:原样可读。
    (child.stdout as Readable).push(
      JSON.stringify({ id: '', msg: { type: 'token_count', info: { total: 42 } } }) + '\n',
    );
    const ev = await agent.nextGlobalEvent({ timeoutMs: 2000 });
    expect(ev?.id).toBe('');
    expect(ev?.msg.type).toBe('token_count');
    // 全局事件不得混入 submission 的事件迭代器。
    const events: EventMsg[] = [];
    for await (const e of await agent.submit('foo')) events.push(e.msg);
    expect(events.some((m) => m.type === 'token_count')).toBe(false);
  }, 10_000);

  it('nextGlobalEvent 超时抛错(对齐 Python queue.Empty)', async () => {
    // 先取走握手事件;之后无新全局事件到达 → 超时抛错而非挂死。
    await agent.nextGlobalEvent({ timeoutMs: 2000 });
    await expect(agent.nextGlobalEvent({ timeoutMs: 100 })).rejects.toThrow('timed out');
  }, 10_000);

  it('nextGlobalEvent 在 stdout EOF 后返回 null', async () => {
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const eofAgent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
    try {
      stdout.write(JSON.stringify({ id: '', msg: {
        type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only',
      } }) + '\n');
      stdout.end();
      // EOF 后:队列中已有的事件仍可读,随后返回 null(对齐 Python None)。
      const handshake = await eofAgent.nextGlobalEvent({ timeoutMs: 2000 });
      expect(handshake?.msg.type).toBe('session_configured');
      expect(await eofAgent.nextGlobalEvent({ timeoutMs: 2000 })).toBeNull();
    } finally {
      await eofAgent.close();
    }
  }, 10_000);

  it('prompt 遇 turn_aborted 抛错(对齐 Python,不静默返回部分文本)', async () => {
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const abortAgent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
    try {
      stdout.write(JSON.stringify({ id: '', msg: {
        type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only',
      } }) + '\n');
      const pending = abortAgent.prompt('hi');
      // 从 stdin 捕获 prompt 内部生成的 submission id,向同 id 通道
      // 注入 delta + turn_aborted。
      const subId = await new Promise<string>((resolve) => {
        stdin.once('data', (c: Buffer) => resolve(JSON.parse(c.toString().trim()).id));
      });
      stdout.write(JSON.stringify({ id: subId, msg: {
        type: 'agent_message_delta', delta: 'part',
      } }) + '\n');
      stdout.write(JSON.stringify({ id: subId, msg: {
        type: 'turn_aborted', turn_id: 't', reason: { type: 'user_interrupt' },
      } }) + '\n');
      await expect(pending).rejects.toThrow('zhuye turn aborted');
    } finally {
      await abortAgent.close();
    }
  }, 10_000);
});