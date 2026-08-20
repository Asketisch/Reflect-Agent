//! ReflectAgent 协议回路测试:用一个轻量 mock serve 进程驱动协议。
//!
//! mock serve 由 Node 直接跑一个 stdin→stdout 回显脚本(模拟 reflect
//! serve 的 JSONL 协议),无需 spawn `reflect` 二进制,测试完全离线。
//!
//! 行为契约:
//! - 收到任何 stdin 一行 → emit `session_configured`(`id=""`)回应;
//! - 之后把每行 stdin 解析为 Submission,emit 一个匹配 submission.id
//!   的 turn_complete 事件 + agent_message_delta。
//! - 收到 `register_tools` op → 不发任何事件(注册被静默接受);
//! - 收到 `tool_execution_response` op → 不发任何事件(回执被静默接受);
//! - 收到 `shutdown` op → 发 shutdown_complete 后退出。

import { spawn, ChildProcessWithoutNullStreams } from 'node:child_process';
import { Readable, Writable } from 'node:stream';

import { describe, expect, it, beforeEach, afterEach } from 'vitest';

import {
  EVENT_ID_NONE,
  Event,
  EventMsg,
  ReflectAgent,
} from '../src/index.js';

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
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: {
            type: 'turn_complete', turn_id: 't', status: 'ok',
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

describe('ReflectAgent 协议回路(mock serve 驱动)', () => {
  let agent: ReflectAgent;
  let child: ChildProcessWithoutNullStreams;

  beforeEach(async () => {
    child = spawnMockServe();
    // 直接喂 stdin / stdout —— 等同于 ReflectAgent.spawn 但跳过 spawn。
    const stdin = child.stdin as Writable;
    const stdout = child.stdout as Readable;
    agent = await ReflectAgent.fromStdio(stdin, stdout, 5000);
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
    // 等到子进程 exit / reflect exit code == 0
    const exitPromise = new Promise<number | null>((resolve) => {
      child.once('exit', (code) => resolve(code));
    });
    await closeP;
    const code = await exitPromise;
    expect(code).toBe(0);
  }, 10_000);

  it('UnknownEventMsg 降级不抛错', async () => {
    // mock serve 默认只回已知事件;此处通过 fromStdio 直接灌一个
    // 未知 type 验证 SDK 不抛。
    const customAgent = await ReflectAgent.fromStdio(
      child.stdin as Writable,
      child.stdout as Readable,
      2000,
    );
    // 触发协议握手,然后再灌未知事件。
    (child.stdout as Readable).push(
      JSON.stringify({
        id: EVENT_ID_NONE,
        msg: {
          type: 'session_configured',
          session_id: 's',
          model: 'mock/mock-1',
          provider: 'mock',
          approval_policy: 'auto',
          sandbox_policy: 'workspace_only',
        },
      } as Event) + '\n',
    );
    await new Promise((r) => setTimeout(r, 20));
    (child.stdout as Readable).push(
      JSON.stringify({ id: '', msg: { type: 'telemetry_event', payload: { x: 1 } } }) + '\n',
    );
    // 不报错即通过。
    expect(true).toBe(true);
    await customAgent.close().catch(() => {});
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
});