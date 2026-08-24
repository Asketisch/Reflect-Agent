// reflect-agent TS SDK 端到端测试(驱动真实 `reflect serve` 二进制)。
//
// 与 sdks/typescript/tests/protocol.test.ts(mock serve)互补:本文件
// spawn **真二进制**,用 mock provider(REFLECT_MODEL=mock)+ mock 脚本
// 驱动确定性回合,覆盖:
//   1. spawn → session_configured 握手;
//   2. prompt 单轮 → mock 脚本文本回流;
//   3. 多轮 prompt 上下文保持;
//   4. registerTool → LLM 调用 → 客户端执行 → 回执 → turn_complete;
//   5. interrupt → turn_aborted;
//   6. close 幂等优雅退出。
//
// 运行:tests/sdk-ts/run.sh(内部 `node --test`),或
//   REFLECT_BIN=... node --test tests/sdk-ts/

import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';

// 直接 import 构建产物 dist(与 scripts/sdk_smoke.sh 同路径约定),
// 避免 vitest 配置;run.sh 会先确保 dist 已构建。
const { ReflectAgent } = await import(
  new URL('../../sdks/typescript/dist/index.js', import.meta.url).href
);

const ROOT = new URL('../../', import.meta.url).pathname;
const REFLECT_BIN = process.env.REFLECT_BIN ?? join(ROOT, 'target/release/reflect');

// 隔离 HOME:预写关闭全部内置 hook 的最小 config.toml(理由同 Python 侧)。
const home = mkdtempSync(join(tmpdir(), 'reflect-ts-home.'));
mkdirSync(join(home, '.reflect'), { recursive: true });
writeFileSync(
  join(home, '.reflect', 'config.toml'),
  '# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n'
);

before(() => {
  process.env.HOME = home;
});
after(() => {
  delete process.env.REFLECT_MODEL;
  delete process.env.REFLECT_MOCK_SCRIPT;
});

/** 构造指向真二进制 + mock provider 的 spawn 选项;写临时 mock 脚本。 */
function makeOptions(scriptLines) {
  const dir = mkdtempSync(join(tmpdir(), 'reflect-ts-script.'));
  const env = { REFLECT_MODEL: 'mock' };
  if (scriptLines) {
    const script = join(dir, 'script.jsonl');
    writeFileSync(script, scriptLines.join('\n') + '\n');
    env.REFLECT_MOCK_SCRIPT = script;
  }
  return { bin: REFLECT_BIN, env };
}

describe('TS SDK × 真二进制', () => {
  it('spawn 完成 session_configured 握手', async () => {
    const agent = await ReflectAgent.spawn(makeOptions());
    await agent.close();
  });

  it('prompt 单轮:mock 脚本文本回流', async () => {
    const agent = await ReflectAgent.spawn(
      makeOptions(['{"type":"text","text":"ts-e2e-reply"}'])
    );
    try {
      assert.equal(await agent.prompt('第一轮'), 'ts-e2e-reply');
    } finally {
      await agent.close();
    }
  });

  it('多轮 prompt 上下文保持', async () => {
    const agent = await ReflectAgent.spawn(
      makeOptions([
        '{"type":"text","text":"first-reply"}',
        '{"type":"text","text":"second-reply"}',
      ])
    );
    try {
      assert.equal(await agent.prompt('第一轮'), 'first-reply');
      assert.equal(await agent.prompt('第二轮'), 'second-reply');
    } finally {
      await agent.close();
    }
  });

  it('registerTool:LLM 调用 → 客户端执行 → 回执 → turn_complete', async () => {
    const calls = [];
    const agent = await ReflectAgent.spawn(
      makeOptions([
        '{"type":"tool_call","name":"sdk_echo","args":{"q":"ping"}}',
        '{"type":"text","text":"tool-done"}',
      ])
    );
    try {
      await agent.registerTool('sdk_echo', 'echo back', { type: 'object' }, (args) => {
        calls.push(args);
        return {
          content: [{ type: 'text', text: `echo:${args.q}` }],
          is_error: false,
          metadata: {},
          elapsed_ms: 0,
        };
      });
      let sawComplete = false;
      for await (const ev of await agent.submit('调工具')) {
        if (ev.msg.type === 'turn_complete') {
          sawComplete = true;
          break;
        }
      }
      assert.deepEqual(calls, [{ q: 'ping' }]);
      assert.ok(sawComplete, '未见 turn_complete');
    } finally {
      await agent.close();
    }
  });

  it('interrupt 打断远程工具等待中的 turn(turn_aborted)', async () => {
    // wire 语义(实测):turn_aborted 挂在 interrupt 这条 submission 自己的
    // id 上 —— 用 submit(op, {submissionId}) 提交并在其迭代器上断言;
    // 原 turn 在回执释放后照常 turn_complete。
    let started = false;
    let release = null;
    const gate = new Promise((r) => (release = r));
    const agent = await ReflectAgent.spawn(
      makeOptions([
        '{"type":"tool_call","name":"slow_tool","args":{}}',
        '{"type":"text","text":"after"}',
      ])
    );
    try {
      await agent.registerTool('slow_tool', 'block until released', { type: 'object' }, async () => {
        started = true;
        await gate;
        return {
          content: [{ type: 'text', text: 'slow result' }],
          is_error: false,
          metadata: {},
          elapsed_ms: 0,
        };
      });
      const turnEvents = await agent.submit('调慢工具');
      // 轮询等 handler 进入等待(turn 阻塞在远程工具上)
      const deadline = Date.now() + 10_000;
      while (!started && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 50));
      }
      if (!started) throw new Error('handler 未被调用');
      const intrEvents = await agent.submit({ type: 'interrupt' }, { submissionId: 'intr-1' });
      const intrTypes = [];
      for await (const ev of intrEvents) intrTypes.push(ev.msg.type);
      release();
      assert.ok(intrTypes.includes('turn_aborted'), `intr=${JSON.stringify(intrTypes)}`);
      const turnTypes = [];
      for await (const ev of turnEvents) turnTypes.push(ev.msg.type);
      assert.ok(turnTypes.includes('turn_complete'), `turn=${JSON.stringify(turnTypes)}`);
    } finally {
      release();
      await agent.close();
    }
  });

  it('close 幂等', async () => {
    const agent = await ReflectAgent.spawn(makeOptions());
    await agent.close();
    await agent.close();
  });
});
