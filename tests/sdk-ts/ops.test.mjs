// reflect-agent TS SDK 深度用例:submit() 全 Op 面 + 审批 + 持久化。
//
// 与 real-bin.test.mjs(核心协议回路)互补,本文件驱动**非 turn 操作**
// 的完整使用路径 —— 这些路径此前没有终态事件,SDK 迭代器会永久挂起,
// v1.3 起由 serve 在 per-turn 通道排空后发 `submission_closed` 收尾:
//   1. 非 turn ops 逐条 submit:compact / set_effort / rewind /
//      set_permission_mode / cycle_permission_mode / enter_goal_mode /
//      exit_goal_mode / enter_plan_mode / exit_plan_mode / plan_approval;
//   2. 权限模式切换的全局事件(id="")不进按 id 路由的迭代器 →
//      改断言 rollout JSONL 里的持久化记录(from/to 轨迹);
//   3. MCP 工具审批全链路(REFLECT_APPROVALS=1):approval_request →
//      `approve(id, 'approve')` → 工具真执行 → turn_complete;
//   4. 孤儿审批回执(无 pending waiter)不崩、会话存活。
//
// 运行:tests/sdk-ts/run.sh,或
//   REFLECT_BIN=... node --test tests/sdk-ts/ops.test.mjs

import { mkdtempSync, writeFileSync, mkdirSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

const { ReflectAgent } = await import(
  new URL('../../sdks/typescript/dist/index.js', import.meta.url).href
);

const ROOT = new URL('../../', import.meta.url).pathname;
const REFLECT_BIN = process.env.REFLECT_BIN ?? join(ROOT, 'target/release/reflect');
const MOCK_MCP = join(ROOT, 'target/debug/mock_mcp_server');

// 迭代器墙钟预算:submission_closed 缺失(回归)时绝不挂死整个套件。
const DRAIN_TIMEOUT_MS = 30_000;

/**
 * 每个用例独立 HOME(与 Python 侧 function 级 fixture 对齐):rollout
 * 断言扫的是该 HOME 的 sessions 目录,跨用例共享 HOME 会让持久化记录
 * 互相污染(如 pmc 轨迹断言)。SDK spawn 把 process.env 合并进子进程
 * env,改 process.env.HOME 即隔离。
 */
function freshHome() {
  const h = mkdtempSync(join(tmpdir(), 'reflect-ts-ops-home.'));
  mkdirSync(join(h, '.reflect'), { recursive: true });
  writeFileSync(
    join(h, '.reflect', 'config.toml'),
    '# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n'
  );
  process.env.HOME = h;
  return h;
}

/** 构造指向真二进制 + mock provider 的 spawn 选项;写临时 mock 脚本。 */
function makeOptions(scriptLines, extraEnv = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'reflect-ts-ops-script.'));
  const env = { REFLECT_MODEL: 'mock', ...extraEnv };
  if (scriptLines) {
    const script = join(dir, 'script.jsonl');
    writeFileSync(script, scriptLines.join('\n') + '\n');
    env.REFLECT_MOCK_SCRIPT = script;
  }
  return { bin: REFLECT_BIN, env };
}

/** 消费 submit() 迭代器,带墙钟超时(防 submission_closed 回归挂死)。 */
async function drain(iterable) {
  const events = [];
  const t0 = Date.now();
  for await (const ev of iterable) {
    events.push(ev);
    if (Date.now() - t0 > DRAIN_TIMEOUT_MS) {
      throw new Error(`迭代器 ${DRAIN_TIMEOUT_MS}ms 未收尾: ${JSON.stringify(events)}`);
    }
  }
  return events;
}

const types = (events) => events.map((e) => e.msg.type);

/** 递归找当前 HOME 下的 rollout JSONL( sessions/YYYY/MM/DD/<thread>.jsonl )。 */
function findRollouts() {
  const base = join(process.env.HOME, '.reflect', 'sessions');
  const out = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (name.endsWith('.jsonl')) out.push(p);
    }
  };
  try {
    walk(base);
  } catch {
    /* sessions 目录尚未创建 */
  }
  return out;
}

function rolloutRecords() {
  const out = [];
  for (const f of findRollouts()) {
    for (const line of readFileSync(f, 'utf8').split('\n')) {
      if (line.trim()) out.push(JSON.parse(line));
    }
  }
  return out;
}

describe('TS SDK × 真二进制:Op 全面', () => {
  it('非 turn ops:submit 迭代器确定性收尾 + 关键事件', async () => {
    freshHome();
    const agent = await ReflectAgent.spawn(
      makeOptions([
        '{"type":"text","text":"t1"}',
        '{"type":"text","text":"t2"}',
        '{"type":"text","text":"t3"}',
      ])
    );
    try {
      // 先来一轮真 turn:给 rewind 留可回退对象。
      const turnEvents = await drain(await agent.submit('第一轮'));
      assert.ok(types(turnEvents).includes('turn_complete'));

      // compact → context_compacted 挂本 submission id,末尾收尾标记。
      let ts = types(await drain(await agent.submit({ type: 'compact' })));
      assert.ok(ts.includes('context_compacted'), `compact: ${ts}`);
      assert.equal(ts.at(-1), 'submission_closed', `compact: ${ts}`);

      // set_effort → 静默写槽:整条只有收尾标记。
      ts = types(await drain(await agent.submit({ type: 'set_effort', effort: 'high' })));
      assert.deepEqual(ts, ['submission_closed'], `set_effort: ${ts}`);

      // rewind → turn_rewound 挂本 submission id。
      ts = types(await drain(await agent.submit({ type: 'rewind' })));
      assert.ok(ts.includes('turn_rewound'), `rewind: ${ts}`);
      assert.equal(ts.at(-1), 'submission_closed');

      // 权限模式 ops:permission_mode_changed 是全局事件(id=""),
      // 不进按 id 路由的迭代器 → 只断言收尾(持久化见下一用例)。
      ts = types(await drain(await agent.submit({ type: 'set_permission_mode', mode: 'plan' })));
      assert.deepEqual(ts, ['submission_closed'], `set_pm: ${ts}`);
      ts = types(await drain(await agent.submit({ type: 'cycle_permission_mode' })));
      assert.deepEqual(ts, ['submission_closed'], `cycle_pm: ${ts}`);

      // goal 模式进出:静默,只有收尾标记。
      ts = types(
        await drain(
          await agent.submit({
            type: 'enter_goal_mode',
            goal: 'probe goal',
            token_budget: 100000,
          })
        )
      );
      assert.deepEqual(ts, ['submission_closed'], `enter_goal: ${ts}`);
      ts = types(await drain(await agent.submit({ type: 'exit_goal_mode' })));
      assert.deepEqual(ts, ['submission_closed'], `exit_goal: ${ts}`);

      // plan 模式 ops:enter → plan_request;exit → plan_ready(fallback);
      // plan_approval(auto_mode)→ 全局 pmc,迭代器只有收尾标记。
      const planEvs = await drain(await agent.submit({ type: 'enter_plan_mode', task: 'probe task' }));
      ts = types(planEvs);
      assert.ok(ts.includes('plan_request'), `enter_plan: ${ts}`);
      assert.equal(ts.at(-1), 'submission_closed');
      const pr = planEvs.find((e) => e.msg.type === 'plan_request').msg;
      assert.equal(pr.task, 'probe task');

      const readyEvs = await drain(await agent.submit({ type: 'exit_plan_mode' }));
      ts = types(readyEvs);
      assert.ok(ts.includes('plan_ready'), `exit_plan: ${ts}`);
      const ready = readyEvs.find((e) => e.msg.type === 'plan_ready').msg;
      assert.ok(ready.markdown, 'plan_ready 应带 fallback markdown');

      ts = types(
        await drain(
          await agent.submit({ type: 'plan_approval', id: ready.plan_id, choice: 'auto_mode' })
        )
      );
      assert.deepEqual(ts, ['submission_closed'], `plan_approval: ${ts}`);
    } finally {
      await agent.close();
    }
  });

  it('权限模式切换 → rollout JSONL 持久化轨迹', async () => {
    freshHome();
    const agent = await ReflectAgent.spawn(
      makeOptions(['{"type":"text","text":"t1"}'])
    );
    try {
      await drain(await agent.submit('持久化探针'));
      await drain(await agent.submit({ type: 'set_permission_mode', mode: 'plan' }));
      await drain(await agent.submit({ type: 'cycle_permission_mode' }));
      await new Promise((r) => setTimeout(r, 500)); // 落盘是 best-effort spawn

      const recs = rolloutRecords();
      assert.ok(recs.some((r) => r.type === 'session_meta'), '无 session_meta');
      assert.ok(recs.some((r) => r.type === 'message'), '无 message');
      const pmc = recs.filter((r) => r.type === 'permission_mode_changed');
      // auto→plan(set)+ plan→auto(cycle)各一条,顺序保持。
      assert.deepEqual(
        pmc.map((r) => [r.from, r.to]),
        [
          ['auto', 'plan'],
          ['plan', 'auto'],
        ],
        `pmc 轨迹: ${JSON.stringify(pmc)}`
      );
      assert.ok(pmc.every((r) => r.at), 'pmc 缺 at');
    } finally {
      await agent.close();
    }
  });

  it('MCP 工具审批全链路:approval_request → approve → 真执行', async () => {
    freshHome();
    // MCP server 配置:echo 工具 required_permission=Prompt → Auto 模式
    // 下必经审批 gate(Auto 模式 auto_approves_tool=False)。
    const cfgPath = join(process.env.HOME, '.reflect', 'config.toml');
    const prev = readFileSync(cfgPath, 'utf8');
    writeFileSync(
      cfgPath,
      prev + `\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "${MOCK_MCP}"\n`
    );
    const agent = await ReflectAgent.spawn(
      makeOptions(
        [
          '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"probe"}}',
          '{"type":"text","text":"approved-done"}',
        ],
        { REFLECT_APPROVALS: '1' }
      )
    );
    try {
      // MCP 注册是异步的:握手 + list_tools 完成前调用会报 unknown tool。
      await new Promise((r) => setTimeout(r, 2000));
      const events = [];
      let approval = null;
      const t0 = Date.now();
      for await (const ev of await agent.submit('调 echo 工具')) {
        events.push(ev);
        if (Date.now() - t0 > DRAIN_TIMEOUT_MS) {
          throw new Error(`审批后 turn 未收尾: ${JSON.stringify(types(events))}`);
        }
        const m = ev.msg;
        if (m.type === 'approval_request') {
          approval = m;
          await agent.approve(m.request_id, 'approve');
        } else if (m.type === 'turn_complete') {
          break;
        }
      }
      const ts = types(events);
      assert.ok(approval, `未见 approval_request: ${ts}`);
      assert.equal(approval.kind.type, 'tool');
      assert.equal(approval.kind.tool_name, 'mcp__fsx__echo');
      // approve 回执接通 oneshot 后 turn 照常收尾。
      assert.ok(ts.includes('turn_complete'), `审批后 turn 未收尾: ${ts}`);
      // 放行后 MCP 工具真执行:tool_call_end 带回显文本 "probe"。
      const tce = events.filter((e) => e.msg.type === 'tool_call_end').map((e) => e.msg);
      assert.ok(tce.length > 0, `未见 tool_call_end: ${ts}`);
      assert.ok(
        tce.some(
          (m) => m.is_error === false && JSON.stringify(m, null, 0).includes('probe')
        ),
        `tool_call_end 未含回显: ${JSON.stringify(tce)}`
      );
    } finally {
      await agent.close();
    }
  });

  it('孤儿审批回执:无 pending waiter 不崩、会话存活', async () => {
    freshHome();
    const agent = await ReflectAgent.spawn(
      makeOptions(['{"type":"text","text":"alive"}'])
    );
    try {
      // 无对应 approval_request 的回执:core 查不到 waiter,静默丢弃。
      await agent.approve('nonexistent-id', 'approve');
      await new Promise((r) => setTimeout(r, 500));
      // 会话仍健康:下一轮 prompt 正常回流。
      assert.equal(await agent.prompt('还在吗'), 'alive');
    } finally {
      await agent.close();
    }
  });
});
