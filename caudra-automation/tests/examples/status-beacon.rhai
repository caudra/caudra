let meta = #{
    name: "status-beacon",
    description: "Publish this worker's status when it changes, checked every 5 minutes",
    triggers: [#{ kind: "schedule", every: "5m" }],
    messaging: #{ publish: ["swarm.status"] },
};
let s = event.session;
let held = s.work.held;
let status = if held == () { s.status } else { s.status + " on " + held.group + "/" + held.work };
if status == state.last { return; }
state.last = status;
publish("swarm.status", s.name + " is " + status);
