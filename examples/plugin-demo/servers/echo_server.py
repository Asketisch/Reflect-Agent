#!/usr/bin/env python3
"""Reflect 示例插件的最小 MCP stdio server —— 零依赖,只用标准库。

实现 MCP 生命周期所需的最小方法集:initialize / tools/list / tools/call。
传输约定为 MCP stdio:每行一个 JSON-RPC 2.0 消息,通知(method 以
`notifications/` 开头)不需应答。
"""

import json
import sys

TOOLS = [
    {
        "name": "echo",
        "description": "原样返回输入文本(演示插件 MCP 工具调用链路)。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "要回显的文本"}
            },
            "required": ["text"],
        },
    }
]


def reply(msg_id, result):
    print(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}), flush=True)


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = req.get("method", "")
        msg_id = req.get("id")
        if method.startswith("notifications/"):
            continue
        if method == "initialize":
            version = req.get("params", {}).get("protocolVersion", "2024-11-05")
            reply(
                msg_id,
                {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "demo-echo", "version": "0.1.0"},
                },
            )
        elif method == "tools/list":
            reply(msg_id, {"tools": TOOLS})
        elif method == "tools/call":
            args = req.get("params", {}).get("arguments", {})
            text = args.get("text", "")
            reply(
                msg_id,
                {
                    "content": [{"type": "text", "text": f"[plugin-demo echo] {text}"}],
                    "isError": False,
                },
            )
        elif msg_id is not None:
            print(
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": msg_id,
                        "error": {"code": -32601, "message": "method not found"},
                    }
                ),
                flush=True,
            )


if __name__ == "__main__":
    main()
