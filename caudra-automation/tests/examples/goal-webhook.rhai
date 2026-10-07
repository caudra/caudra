let meta = #{
    name: "goal-webhook",
    description: "Post goal outcomes and record blockers when a goal is impossible",
    triggers: [#{ kind: "goal_finished" }],
    network: ["https://hooks.example.com"],
    secrets: ["HOOK_TOKEN"],
    arm: "always",
};
let response = http(#{
    method: "POST",
    url: "https://hooks.example.com/caudra",
    bearer_env: "HOOK_TOKEN",
    json: #{ session: event.session.title, verdict: event.verdict, reason: event.reason },
});
if event.verdict == "impossible" && response.status == 200 {
    message("The goal was judged impossible. Record what blocked it in BLOCKERS.md.", #{ attach: event });
}
