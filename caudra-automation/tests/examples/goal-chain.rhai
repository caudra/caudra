let meta = #{
    name: "goal-chain",
    description: "Pursue a list of goals in order, starting with the first when armed",
    triggers: [#{ kind: "armed" }, #{ kind: "goal_finished" }],
    args: #{
        goals: #{ type: "list", min: 1, description: "Goal conditions, in order" },
        continuation_limit: #{ type: "int", default_value: 24, min: 1, max: 100 },
    },
};
let done = state.done ?? [];
if event.trigger == "goal_finished" && event.condition == state.current {
    state.current = ();
    if event.verdict != "met" {
        notify("goal-chain stopped: verdict " + event.verdict);
        return;
    }
    done.push(event.condition);
    state.done = done;
} else if event.session.goal != () {
    skip("another goal is active");
}
let next = args.goals.find(|goal| !(goal in done));
if next == () {
    notify("goal-chain: all " + args.goals.len() + " goals are met");
    return;
}
state.current = set_goal(next, #{ continuation_limit: args.continuation_limit }).condition;
