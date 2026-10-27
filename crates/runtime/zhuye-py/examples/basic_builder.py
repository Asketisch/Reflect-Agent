#!/usr/bin/env python3
"""ZhuyeBuilder 基础用法示例(无需 API key 的 describe 路径)."""

from zhuye_py import ZhuyeBuilder


def main() -> None:
    builder = (
        ZhuyeBuilder("openai/gpt-4o")
        .workspace(".")
        .approvals(True)
        .plan_mode(False)
    )
    model, workspace, approvals, plan_mode = builder.describe()
    print("ZhuyeBuilder snapshot:")
    print(f"  model      = {model}")
    print(f"  workspace  = {workspace}")
    print(f"  approvals  = {approvals}")
    print(f"  plan_mode  = {plan_mode}")

    # 若已设置 OPENAI_API_KEY / ANTHROPIC_API_KEY,可取消注释:
    # agent = builder.build()
    # print(f"Built agent model={agent.model()} workspace={agent.workspace()}")


if __name__ == "__main__":
    main()
