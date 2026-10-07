let meta = #{
    name: "task-tracker",
    description: "React to the outcomes of tasks this coordinator published",
    triggers: [#{ kind: "work_finished", groups: ["swarm-tasks"], states: ["completed", "failed", "paused"] }],
    limits: #{ max_per_hour: 30 },
};
if event.state == "completed" {
    message("A swarm task finished. Check its result, then publish follow-up tasks to swarm.tasks if any remain.", #{ attach: event });
} else {
    notify("Swarm task " + event.work + " is " + event.state + " after " + event.attempts + " attempts");
}
