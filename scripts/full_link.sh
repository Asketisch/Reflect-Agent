#!/usr/bin/env bash
# 全链路集成测试 —— 模拟 LLM(script 回放),贯通引擎完整链路。
#
# 链路:Submission(Op)→ submission_loop → 4 节点 StateGraph →
# 工具执行队列(真实 bash/echo/call_<role>)→ 协议事件流断言 →
# rollout 持久化与回放。唯一被 mock 的是 LLM。
#
# 覆盖矩阵见 crates/runtime/reflect-core/tests/full_link.rs 头注:
# 生命周期/工具环/用量、多轮×持久化回填、Rewind、真中断+恢复、
# Steer 边界合并、工具输出流式增量、子代理状态查询、子代理端到端
# (进度推送+结果回流+状态中心)、压缩升级。
#
# 用法:./scripts/full_link.sh
set -euo pipefail
cd "$(dirname "$0")/.."

echo "── reflect 全链路测试(模拟 LLM,离线)"
REFLECT_MODEL=mock cargo test -p reflect-core --test full_link -- --test-threads=4

echo "── 配套引擎切片(中断 / 转向 / 会话事件 / 状态查询)"
REFLECT_MODEL=mock cargo test -p reflect-core \
    --test interrupt --test steer_mid_turn --test session_events \
    --test subagent_status -- --test-threads=4

echo "[ok] 全链路测试全部通过"
