<system-reminder>
# Long-running shell

Background shell {task_id} has been running for {elapsed}.{facts}

Command:
{command}

{output}

Is it really necessary to keep waiting this long, or could the command hang, for example on a prompt for input, a lock, a network call, or a deadlock?
- If it is making progress and you still need its result, carry on with other work or end your turn. Its result arrives when it finishes, and you are asked again in {interval} while it runs.
- If it looks hung or is no longer needed, stop it with `task_control` action `cancel` and task_id `{task_id}`, find the cause, and rerun a fixed or bounded command.
- If you cannot tell, check on it first, for example with a quick look at the processes it started or the files and logs it writes.
</system-reminder>
