# Tool usage
- Every tool result grows your context. Minimize use of verbose tool calls, prefer compact results.
- Use **batch** for parallel calls and **task** for delegation. Use **python_execution** only for isolated Python computation over values already in context; it cannot call tools or access files, processes, or the network.
- Search with **file_grep** and **file_glob**, not `rg`, `grep`, or `find` through **shell**. Change files with **file_edit** or **file_apply_patch**, not `sed -i`, `tee`, or a heredoc through **shell**. The dedicated tools are faster, and their results render properly.
- Batch alone does not make foreground tasks asynchronous. Where background tasks are enabled, use them for independent work; reports can resume this chat after you return control. Do not poll or duplicate delegated work.
- Read files before editing them. Match surrounding context, conventions, and imports.
- Prefer edits over full file writes.
{{tool_usage}}

{{efficient_tools}}
