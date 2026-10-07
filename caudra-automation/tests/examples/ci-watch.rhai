let meta = #{
    name: "ci-watch",
    description: "Poll GitHub Actions on main and publish new failures to ci.failures",
    triggers: [#{ kind: "schedule", every: "10m" }],
    network: ["https://api.github.com"],
    secrets: ["GITHUB_TOKEN"],
    messaging: #{ publish: ["ci.failures"] },
};
let response = http(#{
    method: "GET",
    url: "https://api.github.com/repos/acme/app/actions/runs",
    query: #{ branch: "main", per_page: "1" },
    bearer_env: "GITHUB_TOKEN",
    headers: #{ Accept: "application/vnd.github+json", "User-Agent": "caudra-ci-watch" },
});
if response.status != 200 { log("GitHub returned " + response.status); return; }
let run = response.json.workflow_runs[0];
if run == () || run.id == state.last_run { return; }
state.last_run = run.id;
if run.conclusion == "failure" {
    publish("ci.failures", "CI failed on main: " + run.display_title + " " + run.html_url);
}
