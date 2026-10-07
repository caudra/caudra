let meta = #{
    name: "research-desk",
    description: "Answer research requests from other sessions with deep-research",
    triggers: [
        #{ kind: "message_received", audiences: ["direct"], consume: true },
        #{ kind: "workflow_finished", workflows: ["deep-research"] },
    ],
    workflows: ["deep-research"],
    messaging: #{ reply: true, send: ["*"] },
};
let requests = state.requests ?? #{};
if event.trigger == "message_received" {
    if event.sender == () || !event.text.starts_with("research:") { release("not a research request"); }
    let run = start_workflow("deep-research", #{ query: event.text.sub_string(9) });
    requests[run.run_id] = #{ to: event.sender, message: event.message_id };
    try {
        reply("Started " + run.name + ". The report follows when it finishes.");
    } catch (err) {
        log("Could not acknowledge the request: " + err.message);
    }
} else if requests.contains(event.run_id) {
    let request = requests.remove(event.run_id);
    send(request.to, "Report from " + event.name + " (" + event.status + ")\n\n" + event.report, #{ reply_to: request.message });
}
state.requests = requests;
