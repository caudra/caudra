let meta = #{
    name: "keep-going",
    description: "Work through a backlog file during work hours",
    triggers: [#{ kind: "idle", after: "2m" }],
    limits: #{ max_per_hour: 6 },
    args: #{
        file: #{ type: "string", default_value: "TODO.md", description: "Backlog with checkboxes" },
        from_hour: #{ type: "int", default_value: 9, min: 0, max: 23 },
        until_hour: #{ type: "int", default_value: 18, min: 1, max: 24 },
    },
};
let t = now();
if (t.weekday in ["sat", "sun"]) || t.hour < args.from_hour || t.hour >= args.until_hour { skip("outside work hours"); }
if event.outcome != "completed" { skip("the last turn ended " + event.outcome); }
if event.last_response.contains("BACKLOG EMPTY") { skip("the backlog is empty"); }
message("Continue with the next unchecked item in " + args.file + ". When none remain, reply with BACKLOG EMPTY.");
