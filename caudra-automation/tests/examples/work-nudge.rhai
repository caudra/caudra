let meta = #{
    name: "work-nudge",
    description: "Ask the agent to report group work it paused without an outcome, once per item",
    triggers: [#{ kind: "idle" }],
    limits: #{ max_per_hour: 6 },
};
let item = event.work.find(|w| w.pause_reason == "completion_required");
if item == () { return; }
if item.work == state.last {
    notify("Work item " + item.work + " in " + item.group + " paused again and needs you");
    return;
}
state.last = item.work;
message("Work item " + item.work + " in group " + item.group + " paused because your turn ended without an outcome. Finish it, then report with work_assignment.");
