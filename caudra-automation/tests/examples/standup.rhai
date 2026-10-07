let meta = #{
    name: "standup",
    description: "Write standup bullets at 09:00 on weekdays and post them to Slack",
    triggers: [
        #{ kind: "schedule", at: "09:00", weekdays: ["mon", "tue", "wed", "thu", "fri"], catch_up: "skip" },
        #{ kind: "idle" },
    ],
    network: ["https://hooks.slack.com"],
    secrets: ["SLACK_STANDUP_URL"],
    timezone: "Europe/Berlin",
};
if event.trigger == "schedule" {
    message("Summarize yesterday's commits in this repository as three standup bullets. Reply with only the bullets.");
} else if (meta.name in event.automations) && event.outcome == "completed" {
    http(#{ method: "POST", url_env: "SLACK_STANDUP_URL", json: #{ text: event.last_response } });
}
