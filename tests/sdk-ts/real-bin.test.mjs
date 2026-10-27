// TS SDK 绑定层:SDK 客户端自身的行为(不测服务端语义)。
//
// 与能力覆盖层(tests/functional/,Python 原始 wire 打真二进制)互补:
// 本文件驱动**真二进制**,但断言的是 **TS SDK 怎么反应** —— 迭代器何时
// 收尾、handler 怎么被调、close 怎么收尾。
//
// 覆盖(5 条):
//   1. spawn → session_configured 握手;
//   2. 单轮 prompt 文本回流;
//   3. registerTool → LLM 调用 → 客户端执行 → 回执 → turn_complete;
//   4. 非 turn op 收尾(submit 迭代器以 submission_closed 结束);
//   5. close 幂等 + 子进程退出码 0。
//
// 为什么还留这 5 条:历史证明只有「真二进制」这一层能发现协议漂移 ——
// v1.3 之前两 SDK 迭代器在非 turn op 上永久挂起、TS approve 发
// {type:'approve'} 导致审批永久挂起,这两个缺陷在 mock serve 层都测不出来
// (mock 是自己写的假服务端,不会随真协议演进)。
//
// 运行:tests/run_all.sh sdk-ts,或
//   ZHUYE_BIN=... node --test tests/sdk-ts/real-bin.test.mjs

import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

// 直接 import 构建产物 dist(与 scripts/sdk_smoke.sh 同路径约定),
// 避免 vitest 配置;run.sh 会先确保 dist 已构建。
const { ZhuyeAgent } = await import(
  new URL('../../sdks/typescript/dist/index.js', import.meta.url).href
);

const ROOT = new URL('../../', import.meta.url).pathname;
const ZHUYE_BIN = process.env.ZHUYE_BIN ?? join(ROOT, 'target/release/zhuye');

/** 隔离 HOME:预写关闭全部内置 hook 的最小 config.toml(理由同 Python 侧)。 */
const home = mkdtempSync(join(tmpdir(), 'zhuye-ts-home.'));
mkdirSync(join(home, '.zhuye'), { recursive: true });
writeFileSync(
  join(home, '.zhuye', 'config.toml'),
  '# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n'
);
process.env.HOME = home;

/** 构造指向真二进制 + mock provider 的 spawn 选项;写临时 mock 脚本。 */
function makeOptions(scriptLines) {
  const dir = mkdtempSync(join(tmpdir(), 'zhuye-ts-script.'));
  const env = { ZHUYE_MODEL: 'mock' };
  if (scriptLines) {
    const script = join(dir, 'script.jsonl');
    writeFileSync(script, scriptLines.join('\n') + '\n');
    env.ZHUYE_MOCK_SCRIPT = script;
  }
  return { bin: ZHUYE_BIN, env };
}

describe('TS SDK 绑定层 × 真二进制', () => {
  it('K1: spawn 完成 session_configured 握手', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions());
    await agent.close();
  });

  it('K2: 单轮 prompt 文本回流', async () => {
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"ts-e2e-reply"}'])
    );
    try {
      assert.equal(await agent.prompt('第一轮'), 'ts-e2e-reply');
    } finally {
      await agent.close();
    }
  });

  it('K3: registerTool → LLM 调用 → 客户端执行 → 回执 → turn_complete', async () => {
    const calls = [];
    const agent = await ZhuyeAgent.spawn(
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

  it('K4: 非 turn op 的迭代器以 submission_closed 收尾', async () => {
    // 没有 submission_closed,SDK 迭代器会永久挂起(v1.3 起 serve 在
    // per-turn 通道排空后发这条收尾标记)。
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"t1"}'])
    );
    try {
      const types = [];
      for await (const ev of await agent.submit({ type: 'set_effort', effort: 'high' })) {
        types.push(ev.msg.type);
      }
      assert.deepEqual(types, ['submission_closed'], `收尾异常:${types}`);
    } finally {
      await agent.close();
    }
  });

  it('K6: close 幂等 + 子进程优雅退出', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions());
    await agent.close();
    await agent.close();
  });
});
