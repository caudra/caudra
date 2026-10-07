let meta = #{
    name: "nightly-review",
    description: "Review the day's commits at 02:00 and post the report to Slack",
    triggers: [
        #{ kind: "schedule", at: "02:00", catch_up: "skip" },
        #{ kind: "workflow_finished", workflows: ["review-changes"] },
    ],
    workflows: ["review-changes"],
    network: ["https://hooks.slack.com"],
    secrets: ["SLACK_REVIEW_URL"],
};
if event.trigger == "schedule" {
    state.run = start_workflow("review-changes", #{ scope: "commits from the last 24 hours on main" }).run_id;
} else if event.run_id == state.run {
    http(#{ method: "POST", url_env: "SLACK_REVIEW_URL", json: #{ text: "Nightly review " + event.status + "\n\n" + event.report } });
}
