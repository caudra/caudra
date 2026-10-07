let meta = #{
    name: "ci-triage",
    description: "Start a root-cause run for each CI failure, instead of waking the model",
    triggers: [#{ kind: "message_received", topics: ["ci.failures"], senders: ["@ci-watcher"], scripts: ["nightly-ci"], consume: true }],
    workflows: ["root-cause"],
    limits: #{ max_per_hour: 2 },
};
start_workflow("root-cause", #{ failure: event.text }, #{ agent_budget: 24 });
