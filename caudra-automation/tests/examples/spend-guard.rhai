let meta = #{
    name: "spend-guard",
    description: "Pause automations once this session has spent $20",
    triggers: [#{ kind: "idle" }],
    arm: "always",
};
if (event.session.cost ?? 0.0) > 20.0 {
    pause_automations("spend-guard: this session passed $20");
    notify("Automations paused: this session has spent over $20.");
}
