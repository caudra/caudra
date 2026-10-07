let meta = #{
    name: "page-me",
    description: "Push a phone notification when the session has waited on me for 10 minutes",
    triggers: [#{ kind: "needs_input", after: "10m" }],
    network: ["https://ntfy.sh"],
    secrets: ["NTFY_URL", "NTFY_TOKEN"],
    arm: "always",
};
let ask = switch event.input {
    "permission" => "approve " + event.tool,
    "plan" => "review a plan",
    "auth" => "sign in again",
    _ => "answer a " + event.input,
};
http(#{
    method: "POST",
    url_env: "NTFY_URL",
    bearer_env: "NTFY_TOKEN",
    headers: #{ Title: "Caudra is waiting" },
    body: event.session.title + " needs you to " + ask,
});
