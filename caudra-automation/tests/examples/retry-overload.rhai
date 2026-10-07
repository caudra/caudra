let meta = #{
    name: "retry-overload",
    description: "Resume after rate-limit and overload errors",
    triggers: [#{ kind: "idle", after: "1m" }],
    limits: #{ max_per_hour: 4 },
};
if event.outcome == "error" && (event.error_kind in ["rate_limit", "overloaded"]) {
    message("The previous turn stopped on a provider error. Continue where you left off.");
}
