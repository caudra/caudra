let meta = #{
    name: "timebox",
    description: "Ask a turn that has worked for an hour to wrap up",
    triggers: [#{ kind: "schedule", every: "5m" }],
    limits: #{ cooldown: "30m" },
};
let minutes = (now().unix - event.session.status_since) / 60;
if event.session.status == "working" && minutes >= 60 {
    notify("Still working after " + minutes + " minutes");
    message("You have worked on this for an hour. Finish the current step, then summarize progress and what remains.", #{ delivery: "guide", expires: "10m" });
}
