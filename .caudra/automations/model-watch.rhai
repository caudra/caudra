// Keeps Caudra's built-in model list current. Arm it in a session that works in
// its own worktree, under autopilot, in Build mode. See CONTRIBUTING.md,
// "Keeping the model list current".
let meta = #{
    name: "model-watch",
    description: "Once a day: merge main into this worktree's branch, survey models.dev and the provider pages, apply confirmed model changes with the model-updates skill, validate, then fast-forward local main",
    triggers: [
        #{ kind: "schedule", every: "30m" },
        #{ kind: "armed" },
        #{ kind: "goal_finished" },
        #{ kind: "idle" },
    ],
    args: #{
        main_worktree: #{ type: "string", example: "/home/me/caudra", description: "Absolute path of the checkout that has main checked out" },
        branch: #{ type: "string", default_value: "model-watch", description: "The branch this worktree has checked out" },
        start_hour: #{ type: "int", default_value: 7, min: 0, max: 23, description: "Local hour from which the daily run may start" },
        first_since: #{ type: "string", default_value: "2026-10-01", description: "Survey start date (YYYY-MM-DD) of the first run. Later runs start at the date of the last run that landed" },
        continuation_limit: #{ type: "int", default_value: 40, min: 1, max: 100, description: "Automatic continuations each step's goal may take" },
        wait_notice_hours: #{ type: "int", default_value: 12, min: 1, max: 168, description: "Hours landing may wait on a busy main checkout before you are told" },
    },
};

let t = now();
let today = t.date;
let step = state.step;
let goal_options = #{ continuation_limit: args.continuation_limit };

switch event.trigger {
    "schedule" => {
        if state.halted == true { skip("halted: arm model-watch again to resume"); }
        if step == "land" {
            if state.landing == true {
                let quiet = event.session.status == "idle" && t.unix - (state.land_sent ?? 0) > 3600;
                if !quiet { skip("landing in progress"); }
                log("the landing request got no answer within an hour, asking again");
            }
            if state.wait_since != () && state.waited_notice != true && t.unix - state.wait_since > args.wait_notice_hours * 3600 {
                state.waited_notice = true;
                notify("model-watch has waited " + args.wait_notice_hours + " hours to land on main, because " + args.main_worktree + " is busy: changed or untracked files, another branch, or an operation in progress.");
            }
            message(land_message(args, state.run));
            state.landing = true;
            state.land_fire = event.fire_id;
            state.land_sent = t.unix;
            return;
        }
        if step != () {
            let gone = event.session.goal == () && event.session.status == "idle" && t.unix - event.session.status_since > 7200;
            if !gone { skip("the " + step + " step is in progress"); }
            state.halted = true;
            notify("model-watch stopped: the " + step + " goal of the " + state.run + " run ended without finishing. Arm model-watch again in /automations to retry the step.");
            return;
        }
        if state.run == today { skip("today's run is done"); }
        if t.hour < args.start_hour { skip("before " + args.start_hour + ":00"); }
        let blocked = blocker(event.session);
        if blocked != () {
            if state.nagged == today { skip(blocked); }
            state.nagged = today;
            notify("model-watch cannot start today's run: " + blocked + ".");
            return;
        }
        state.run = today;
        state.since = state.last_landed ?? args.first_since;
        state.step = "sync";
        state.current = set_goal(goal_for("sync", args, state.run, state.since), goal_options).condition;
    }
    "goal_finished" => {
        if state.current == () || event.condition != state.current { skip("not a model-watch goal"); }
        state.current = ();
        if event.verdict != "met" {
            state.halted = true;
            notify("model-watch stopped at the " + step + " step of the " + state.run + " run: the goal was " + event.verdict + ". Fix what blocked it, then arm model-watch again in /automations.");
            return;
        }
        let next = if step == "sync" && state.resync != true {
            "survey"
        } else if step == "survey" {
            "apply"
        } else if step == "apply" || step == "sync" {
            "validate"
        } else {
            "land"
        };
        state.step = next;
        if next != "land" {
            state.current = set_goal(goal_for(next, args, state.run, state.since), goal_options).condition;
            return;
        }
        state.resync = ();
        message(land_message(args, state.run));
        state.landing = true;
        state.land_fire = event.fire_id;
        state.land_sent = t.unix;
    }
    "idle" => {
        if step != "land" || state.landing != true || event.started_by.fire_id != state.land_fire {
            skip("not a landing turn");
        }
        state.landing = ();
        if event.outcome != "completed" {
            if event.error_kind in ["rate_limit", "overloaded", "timeout", "network"] {
                skip("the landing turn hit a passing error, the next check asks again");
            }
            state.halted = true;
            notify("model-watch could not land the " + state.run + " run: the turn ended " + event.outcome + ". Arm model-watch again in /automations to retry.");
            return;
        }
        let reply = event.last_response;
        reply.trim();
        if reply.ends_with("MODEL-WATCH LAND: WAIT") {
            if state.wait_since == () { state.wait_since = t.unix; }
            skip("the main checkout is busy, the next check asks again");
        }
        if reply.ends_with("MODEL-WATCH LAND: STALE") {
            state.resync = true;
            state.step = "sync";
            state.current = set_goal(goal_for("sync", args, state.run, state.since), goal_options).condition;
            return;
        }
        let landed = reply.ends_with("MODEL-WATCH LAND: DONE");
        if !landed && !reply.ends_with("MODEL-WATCH LAND: NOTHING") {
            state.halted = true;
            notify("model-watch could not land the " + state.run + " run. The transcript says why. Arm model-watch again in /automations to retry.");
            return;
        }
        let review = reply.contains("MODEL-WATCH REVIEW");
        state.last_landed = state.run;
        state.step = ();
        state.land_fire = ();
        state.land_sent = ();
        state.wait_since = ();
        state.waited_notice = ();
        let report = "target/model-watch/" + state.run + ".md in the model-watch worktree";
        if landed && review {
            notify("model-watch landed the " + state.run + " model updates on main, and some need a person: see " + report + ".");
        } else if landed {
            notify("model-watch landed the " + state.run + " model updates on main. Report: " + report + ".");
        } else if review {
            notify("model-watch found nothing to land on " + state.run + ", and some findings need a person: see " + report + ".");
        } else {
            log("nothing to land");
        }
    }
    "armed" => {
        if event.reason != "manual" {
            if step == "land" { state.landing = (); }
            skip("armed on " + event.reason + ", the next check carries on");
        }
        state.halted = ();
        if step == "land" {
            message(land_message(args, state.run));
            state.landing = true;
            state.land_fire = event.fire_id;
            state.land_sent = t.unix;
            return;
        }
        let blocked = blocker(event.session);
        if blocked != () {
            notify("model-watch cannot run: " + blocked + ". Fix that, then arm it again.");
            return;
        }
        if step == () {
            state.run = today;
            state.since = state.last_landed ?? args.first_since;
            step = "sync";
            state.step = step;
        }
        state.current = set_goal(goal_for(step, args, state.run, state.since), goal_options).condition;
    }
}

/// Why a run cannot start in this session, or `()`.
fn blocker(session) {
    if session.goal != () { return "another goal is active"; }
    if session.autopilot == () { return "autopilot is not engaged, run /autopilot"; }
    if session.mode != "build" { return "the session is not in Build mode"; }
    ()
}

fn goal_for(step, a, run, since) {
    let report = "target/model-watch/" + run + ".md";
    let lead = "model-watch run " + run + ", ";
    switch step {
        "sync" => lead + "step 1 of 4, sync: this worktree is on branch " + a.branch + " with no uncommitted changes, local main is an ancestor of HEAD (`git merge-base --is-ancestor main HEAD` succeeds), and `make check` passes. Get there by running `git merge main` in this worktree and resolving every conflict on " + a.branch + ", keeping the intent of both sides. Commit leftovers from an earlier run if they are finished model updates, and revert them otherwise. Change nothing in " + a.main_worktree + ".",
        "survey" => lead + "step 2 of 4, survey: " + report + " lists every model change since " + since + " that `make model-watch ARGS='-- --since " + since + "'` reports or that a source in the model-updates skill announces. Each row names the provider's own page and is marked apply or unconfirmed, and a \"For a person\" section lists what is unconfirmed. Load the model-updates skill first, and change no code in this step.",
        "apply" => lead + "step 3 of 4, apply: every apply row of " + report + " is applied following the model-updates skill and committed on " + a.branch + ", with release notes and `make gen-docs`, and the report says for each row what was done or why not. `git status` is clean.",
        _ => lead + "step 4 of 4, validate: either `git rev-list --count main..HEAD` prints 0, or `make lint`, `make test`, and `make gen-docs-check` all pass on a clean HEAD of " + a.branch + ". A change that cannot be made to pass is reverted with a commit and listed under \"For a person\" in " + report + ".",
    }
}

fn land_message(a, run) {
    let main = a.main_worktree;
    "Land the model-watch run " + run + " on local main. Check these in order, stop at the first that holds, and end your reply with its marker as the last line, in plain text without backticks or bold.\n"
        + "1. `git rev-list --count main.." + a.branch + "` prints 0: MODEL-WATCH LAND: NOTHING\n"
        + "2. `git -C " + main + " status --porcelain` prints anything, `git -C " + main + " branch --show-current` is not main, or `git -C " + main + " status` reports a merge, rebase, cherry-pick, revert, or bisect in progress: MODEL-WATCH LAND: WAIT\n"
        + "3. `git merge-base --is-ancestor main " + a.branch + "` fails: MODEL-WATCH LAND: STALE\n"
        + "4. Otherwise run `git -C " + main + " merge --ff-only " + a.branch + "`. When it succeeds: MODEL-WATCH LAND: DONE. When it fails: MODEL-WATCH LAND: FAILED, then the error on the same line.\n"
        + "Put the line MODEL-WATCH REVIEW just before the marker when target/model-watch/" + run + ".md lists anything for a person. Never push. Never commit, stash, reset, or resolve anything in " + main + ": conflicts are resolved on " + a.branch + " only, in a later sync step."
}
