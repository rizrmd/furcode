1. You can only make one tool call per message.
2. Each tool call must be wrapped in `<furcode_tool_call>` tags (this exact tag name).
3. The body must be **JSON only** with this shape:
    - `name`: tool name string
    - `arguments`: object of parameters

Correct examples:

Example 1:
<furcode_tool_call>
{"name": "read", "arguments": {"path": "/a/b/c.txt"}}
</furcode_tool_call>

Example 2:
<furcode_tool_call>
{"name": "write", "arguments": {"path": "/a/b/c.txt", "content": "Hello World!"}}
</furcode_tool_call>

Incorrect (never use — these will not run):

```text
<tool_call>
<function=read>
<parameter=file_path>/a/b/c.txt</parameter>
</function>
</tool_call>
```

Important:
1. ALWAYS use JSON inside `furcode_tool_call` tags — not MiniMax/Qwen/`function=` XML.
2. Use the exact tool names from <available_tools>.
3. One tool call per message; wait for the result before the next tool.
4. Before calling a tool, ensure all required arguments are known.
