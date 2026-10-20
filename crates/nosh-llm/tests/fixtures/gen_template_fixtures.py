#!/usr/bin/env python3
"""Generate official reference text and nosh's compact-tool-layout expectations.

The vendored MiniCPM5 template is not modified. `official_expected` retains its
HF-compatible rendering; `expected` replaces only its tool-definition block.

    python3 gen_template_fixtures.py > template_cases.json
"""
import json
import argparse
import hashlib
import os
import sys

from jinja2.ext import loopcontrols
from jinja2.sandbox import ImmutableSandboxedEnvironment

HERE = os.path.dirname(os.path.abspath(__file__))


def tojson(x, ensure_ascii=False, indent=None, separators=None, sort_keys=False):
    return json.dumps(x, ensure_ascii=ensure_ascii, indent=indent, separators=separators, sort_keys=sort_keys)


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--chat-format", choices=("minicpm5", "qwen35"), default="minicpm5")
parser.add_argument("--template", help="Qwen Jinja source at the pinned official revision")
args = parser.parse_args()

env = ImmutableSandboxedEnvironment(trim_blocks=True, lstrip_blocks=True, extensions=[loopcontrols])
env.filters["tojson"] = tojson
def raise_exception(message):
    raise ValueError(message)
env.globals["raise_exception"] = raise_exception
if args.chat_format == "qwen35" and not args.template:
    parser.error("--template must identify the pinned Qwen chat_template.jinja")
with open(args.template or os.path.join(HERE, "minicpm5_chat_template.jinja"), "rb") as f:
    template_bytes = f.read()
if args.chat_format == "qwen35":
    assert hashlib.sha256(template_bytes).hexdigest() == "273d8e0e683b885071fb17e08d71e5f2a5ddfb5309756181681de4f5a1822d80"
template = env.from_string(template_bytes.decode("utf-8"))

RUN = {
    "type": "function",
    "function": {
        "name": "exec",
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
            {"role": "assistant", "content": "我先看看端口。", "tool_calls": [call("exec", command="ss -ltnp 'sport = :8080'")]},
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
            {"role": "assistant", "content": "", "tool_calls": [call("exec", command="ls | wc -l", timeout_sec=30), call("exec", command="echo <done> && printf 'a\\nb'")]},
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


def compact_layout(case, official):
    tools = case["tools"]
    if not tools:
        return official
    if args.chat_format == "qwen35":
        definitions = "# Tools\n\n<tools>" + "".join("\n" + tojson(tool) for tool in tools)
        definitions += (
            "\n</tools>\n\nTool calls:\n<tool_call>\n<function=function-name>\n"
            "<parameter=parameter-name>\nvalue\n</parameter>\n</function>\n</tool_call>\n"
            "Include all required parameters. Array and object values must be JSON. "
            "Values may span multiple lines. Do not add text after a tool call."
        )
        system = case["messages"][0]["content"].strip() if case["messages"][0]["role"] == "system" else ""
        if "<tool_def_sep>" in system:
            body = system.replace("<tool_def_sep>", definitions)
        else:
            body = definitions + ("\n\n" + system if system else "")
        rest = official.split("<|im_end|>\n", 1)[1]
        return "<|im_start|>system\n" + body + "<|im_end|>\n" + rest
    tool_only = render(dict(
        tools=tools, messages=[{"role": "user", "content": ""}],
        add_generation_prompt=False, enable_thinking=False,
    ))
    old_block = tool_only.split("<|im_start|>system\n", 1)[1].split("<|im_end|>\n", 1)[0]
    assert official.count(old_block) == 1
    guidance = (
        "# Tools\n\nTool calls:\n"
        '<function name="function-name"><param name="param-name">param-value</param></function>\n'
        "Wrap values containing <, & or newlines in <![CDATA[...]]>.\n\n<tools>"
    )
    new_block = guidance + "".join("\n" + tojson(tool) for tool in tools) + "\n</tools>"
    return official.replace(old_block, new_block, 1)


out = []
for case in CASES:
    c = dict(case)
    c["official_expected"] = render(case)
    c["expected"] = compact_layout(case, c["official_expected"])
    if args.chat_format == "qwen35":
        c["template_revision"] = "2fc06364715b967f1860aea9cf38778875588b17"
        c["template_sha256"] = hashlib.sha256(template_bytes).hexdigest()
    out.append(c)
json.dump(out, sys.stdout, ensure_ascii=False, indent=1)
sys.stdout.write("\n")
