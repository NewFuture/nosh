#!/usr/bin/env python3
"""Regenerates template_cases.json by rendering the official MiniCPM5
chat_template.jinja the way HF `apply_chat_template` does (jinja2 sandbox,
trim_blocks + lstrip_blocks, json.dumps-based tojson).

    python3 gen_template_fixtures.py > template_cases.json
"""
import json
import os
import sys

from jinja2.ext import loopcontrols
from jinja2.sandbox import ImmutableSandboxedEnvironment

HERE = os.path.dirname(os.path.abspath(__file__))


def tojson(x, ensure_ascii=False, indent=None, separators=None, sort_keys=False):
    return json.dumps(x, ensure_ascii=ensure_ascii, indent=indent, separators=separators, sort_keys=sort_keys)


env = ImmutableSandboxedEnvironment(trim_blocks=True, lstrip_blocks=True, extensions=[loopcontrols])
env.filters["tojson"] = tojson
with open(os.path.join(HERE, "minicpm5_chat_template.jinja"), encoding="utf-8") as f:
    template = env.from_string(f.read())

RUN = {
    "type": "function",
    "function": {
        "name": "run_command",
        "description": "Run a bash command in the user's shell session.",
        "parameters": {
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command line."},
                "timeout_sec": {"type": "integer", "description": "Timeout in seconds (default 60)."},
            },
            "required": ["command"],
        },
    },
}
READ = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": "Read a text file with line numbers; \"path\" may be relative.",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "start_line": {"type": "integer"}},
            "required": ["path"],
        },
    },
}


def call(name, **args):
    return {"type": "function", "function": {"name": name, "arguments": args}}


SYS = "You are nosh, an AI shell running fully offline on the user's computer.\n<tool_def_sep>\n# Rules\n1. Act through tools."

CASES = [
    {
        "name": "agent_multi_turn",
        "tools": [RUN, READ],
        "messages": [
            {"role": "system", "content": SYS},
            {"role": "user", "content": "[task trigger=hash cwd=/home/u/proj]\n刚才为什么启动失败？"},
            {"role": "assistant", "content": "我先看看端口。", "tool_calls": [call("run_command", command="ss -ltnp 'sport = :8080'")]},
            {"role": "tool", "content": "[exit_code=0 duration=0.02s truncated=no]\n--- stdout ---\nLISTEN 0 511 *:8080\n--- stderr ---\n(empty)"},
            {"role": "assistant", "content": "8080 端口被 **node** 占用。"},
            {"role": "user", "content": "再把它结束掉"},
        ],
        "add_generation_prompt": True,
        "enable_thinking": False,
    },
    {
        "name": "parallel_tool_results_merge",
        "tools": [RUN],
        "messages": [
            {"role": "system", "content": SYS},
            {"role": "user", "content": "count files"},
            {"role": "assistant", "content": "", "tool_calls": [call("run_command", command="ls | wc -l", timeout_sec=30), call("run_command", command="echo <done> && printf 'a\\nb'")]},
            {"role": "tool", "content": "3"},
            {"role": "tool", "content": "<done>\na\nb"},
        ],
        "add_generation_prompt": True,
        "enable_thinking": False,
    },
    {
        "name": "system_without_separator",
        "tools": [READ],
        "messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "hi"},
        ],
        "add_generation_prompt": True,
        "enable_thinking": True,
    },
    {
        "name": "no_tools",
        "tools": None,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Say hello in 中文."},
            {"role": "assistant", "content": "你好！"},
            {"role": "user", "content": "thanks"},
        ],
        "add_generation_prompt": True,
        "enable_thinking": False,
    },
    {
        "name": "tools_without_system",
        "tools": [RUN],
        "messages": [{"role": "user", "content": "pwd?"}],
        "add_generation_prompt": False,
        "enable_thinking": None,
    },
]


def render(case):
    kwargs = dict(
        messages=case["messages"],
        tools=case["tools"],
        bos_token="<s>",
        eos_token="</s>",
        add_generation_prompt=case["add_generation_prompt"],
    )
    if case["enable_thinking"] is not None:
        kwargs["enable_thinking"] = case["enable_thinking"]
    return template.render(**kwargs)


out = []
for case in CASES:
    c = dict(case)
    c["expected"] = render(case)
    out.append(c)
json.dump(out, sys.stdout, ensure_ascii=False, indent=1)
sys.stdout.write("\n")
