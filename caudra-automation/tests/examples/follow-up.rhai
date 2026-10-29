let meta = #{
    name: "follow-up",
    description: "Send one follow-up after twenty minutes, then disarm",
    triggers: [#{ kind: "schedule", every: "20m" }],
    limits: #{ max_per_hour: 1 },
    args: #{
        text: #{ type: "string", description: "The follow-up instruction", example: "Check whether the test run has finished." },
    },
};
message(args.text);
done("sent");
