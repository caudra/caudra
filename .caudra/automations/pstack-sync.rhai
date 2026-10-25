// Keeps the bundled pstack current and translated for Caudra. Arm it in a
// session that works in its own worktree, under autopilot, in Build mode. See
// CONTRIBUTING.md, "Keeping pstack current".
let meta = #{
    name: "pstack-sync",
    description: "Once a day: merge main into this worktree's branch, pull cursor/plugins, sync the bundled pstack, retranslate what changed with the pstack-translate skill, validate, then fast-forward local main",
    triggers: [
        #{ kind: "schedule", every: "30m" },
        #{ kind: "armed" },
        #{ kind: "goal_finished", verdicts: ["met", "impossible", "cleared", "stalled"] },
        #{ kind: "idle" },
    ],
    args: #{
        main_worktree: #{ type: "string", example: "/home/me/caudra", description: "Absolute path of the checkout that has main checked out" },
        plugins_checkout: #{ type: "string", example: "/home/me/caudra/external_repos/cursor-plugins", description: "Absolute path of a cursor/plugins clone with its default branch checked out" },
        branch: #{ type: "string", default_value: "pstack-sync", description: "The branch this worktree has checked out" },
        start_hour: #{ type: "int", default_value: 7, min: 0, max: 23, description: "Local hour from which the daily run may start" },
        continuation_limit: #{ type: "int", default_value: 40, min: 1, max: 100, description: "Automatic continuations each step's goal may take" },
        wait_notice_hours: #{ type: "int", default_value: 12, min: 1, max: 168, description: "Hours landing may wait on a busy main checkout before you are told" },
        step_notice_hours: #{ type: "int", default_value: 6, min: 1, max: 168, description: "Hours a step's goal may stay open before you are told it may be looping" },
    },
};

let t = now();
let today = t.date;
let step = state.step;
let goal_options = #{ continuation_limit: args.continuation_limit };

switch event.trigger {
    "schedule" => {
        if state.halted == true { skip("halted: arm pstack-sync again to resume"); }
        if step == "land" {
            if state.landing == true {
                let quiet = event.session.status == "idle" && t.unix - (state.land_sent ?? 0) > 3600;
                if !quiet { skip("landing in progress"); }
                log("the landing request got no answer within an hour, asking again");
            }
            if state.wait_since != () && state.waited_notice != true && t.unix - state.wait_since > args.wait_notice_hours * 3600 {
                state.waited_notice = true;
                notify("pstack-sync has waited " + args.wait_notice_hours + " hours to land on main, because " + args.main_worktree + " is busy: changed or untracked files, another branch, or an operation in progress.");
            }
            message(land_message(args, state.run));
            state.landing = true;
            state.land_fire = event.fire_id;
            state.land_sent = t.unix;
            return;
        }
        if step != () {
            let gone = event.session.goal == () && event.session.status == "idle" && t.unix - event.session.status_since > 7200;
            if !gone {
                let started = state.step_at ?? t.unix;
                if state.stuck_notice != started && t.unix - started > args.step_notice_hours * 3600 {
                    state.stuck_notice = started;
                    notify("pstack-sync's " + step + " step of the " + state.run + " run has had its goal open for over " + args.step_notice_hours + " hours, so it may be looping. Check the session: /goal clear stops the step, and arming pstack-sync again retries it.");
                    return;
                }
                skip("the " + step + " step is in progress");
            }
            state.halted = true;
            notify("pstack-sync stopped: the " + step + " goal of the " + state.run + " run ended without finishing. Arm pstack-sync again in /automations to retry the step.");
            return;
        }
        if state.run == today { skip("today's run is done"); }
        if t.hour < args.start_hour { skip("before " + args.start_hour + ":00"); }
        let blocked = blocker(event.session);
        if blocked != () {
            if state.nagged == today { skip(blocked); }
            state.nagged = today;
            notify("pstack-sync cannot start today's run: " + blocked + ".");
            return;
        }
        state.run = today;
        state.step = "sync";
        state.current = set_goal(goal_for("sync", args, state.run), goal_options).condition;
        state.step_at = t.unix;
    }
    "goal_finished" => {
        if state.current == () || event.condition != state.current { skip("not a pstack-sync goal"); }
        if event.verdict == "stalled" {
            if state.step_at != () && state.stuck_notice == state.step_at { skip("already told that this step is stuck"); }
            state.stuck_notice = state.step_at;
            notify("pstack-sync's " + step + " step of the " + state.run + " run stopped making progress, and its goal stays open. Check the session: a reply there lets it go on, /goal clear stops the step, and arming pstack-sync again retries it.");
            return;
        }
        state.current = ();
        if event.verdict != "met" {
            state.halted = true;
            notify("pstack-sync stopped at the " + step + " step of the " + state.run + " run: the goal was " + event.verdict + ". Fix what blocked it, then arm pstack-sync again in /automations.");
            return;
        }
        let next = if step == "sync" && state.resync != true {
            "pull"
        } else if step == "pull" {
            "translate"
        } else if step == "translate" || step == "sync" {
            "validate"
        } else {
            "land"
        };
        state.step = next;
        if next != "land" {
            state.current = set_goal(goal_for(next, args, state.run), goal_options).condition;
            state.step_at = t.unix;
            return;
        }
        state.resync = ();
        state.step_at = ();
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
            notify("pstack-sync could not land the " + state.run + " run: the turn ended " + event.outcome + ". Arm pstack-sync again in /automations to retry.");
            return;
        }
        let reply = event.last_response;
        reply.trim();
        if reply.ends_with("PSTACK-SYNC LAND: WAIT") {
            if state.wait_since == () { state.wait_since = t.unix; }
            skip("the main checkout is busy, the next check asks again");
        }
        if reply.ends_with("PSTACK-SYNC LAND: STALE") {
            state.resync = true;
            state.step = "sync";
            state.current = set_goal(goal_for("sync", args, state.run), goal_options).condition;
            state.step_at = t.unix;
            return;
        }
        let landed = reply.ends_with("PSTACK-SYNC LAND: DONE");
        if !landed && !reply.ends_with("PSTACK-SYNC LAND: NOTHING") {
            state.halted = true;
            notify("pstack-sync could not land the " + state.run + " run. The transcript says why. Arm pstack-sync again in /automations to retry.");
            return;
        }
        let review = reply.contains("PSTACK-SYNC REVIEW");
        state.step = ();
        state.land_fire = ();
        state.land_sent = ();
        state.wait_since = ();
        state.waited_notice = ();
        let report = "target/pstack-sync/" + state.run + ".md in the pstack-sync worktree";
        if landed && review {
            notify("pstack-sync landed the " + state.run + " pstack update on main, and some of it needs a person: see " + report + ".");
        } else if landed {
            notify("pstack-sync landed the " + state.run + " pstack update on main. Report: " + report + ".");
        } else if review {
            notify("pstack-sync found nothing to land on " + state.run + ", and some findings need a person: see " + report + ".");
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
            notify("pstack-sync cannot run: " + blocked + ". Fix that, then arm it again.");
            return;
        }
        if step == () {
            state.run = today;
            step = "sync";
            state.step = step;
        }
        state.current = set_goal(goal_for(step, args, state.run), goal_options).condition;
        state.step_at = t.unix;
    }
}

/// Why a run cannot start in this session, or `()`.
fn blocker(session) {
    if session.goal != () { return "another goal is active"; }
    if session.autopilot == () { return "autopilot is not engaged, run /autopilot"; }
    if session.mode != "build" { return "the session is not in Build mode"; }
    ()
}

fn goal_for(step, a, run) {
    let report = "target/pstack-sync/" + run + ".md";
    let lead = "pstack-sync run " + run + ", ";
    switch step {
        "sync" => lead + "step 1 of 4, sync: this worktree is on branch " + a.branch + " with no uncommitted changes, local main is an ancestor of HEAD (`git merge-base --is-ancestor main HEAD` succeeds), and `make check` passes. Get there by running `git merge main` in this worktree and resolving every conflict on " + a.branch + ", keeping the intent of both sides. Commit leftovers from an earlier run if they are a finished pstack update, and revert them otherwise. Change nothing in " + a.main_worktree + ".",
        "pull" => lead + "step 2 of 4, pull: `git -C " + a.plugins_checkout + " pull --ff-only` has succeeded, `make sync-pstack ARGS='--source " + a.plugins_checkout + "'` has run in this worktree, and " + report + " records the cursor/plugins commit and every TRANSLATE, NEW, ORPHAN, and PORT line it printed. A sync that changes nothing under caudra-pstack/upstream/ meets this step with a report that says so. Change no translation or code in this step, and change nothing in " + a.plugins_checkout + " beyond the pull.",
        "translate" => lead + "step 3 of 4, translate: every line of " + report + " is settled following the pstack-translate skill, which you load first, and the report says for each what was done. `cargo test -p caudra-pstack` passes, so no served page names a Cursor term and every translation is marked with `python3 scripts/sync-pstack.py --mark`. The update is committed on " + a.branch + " as `chore(pstack): sync cursor/plugins <short commit>`, with a release-notes bullet and `make gen-docs` when users would see the change (a skill, command, or profile added or removed, a ported script's behaviour). `git status` is clean. A run whose sync changed nothing meets this step as it is. This step needs no decision from a person: a translation the skill does not cover gets the closest reading, and it is listed under \"For a person\" in the report without holding the step open.",
        _ => lead + "step 4 of 4, validate: either `git rev-list --count main..HEAD` prints 0, or `make lint`, `make test`, and `make gen-docs-check` all pass on a clean HEAD of " + a.branch + ". A change that cannot be made to pass is reverted with a commit and listed under \"For a person\" in " + report + ".",
    }
}

fn land_message(a, run) {
    let main = a.main_worktree;
    "Land the pstack-sync run " + run + " on local main. Check these in order, stop at the first that holds, and end your reply with its marker as the last line, in plain text without backticks or bold.\n"
        + "1. `git rev-list --count main.." + a.branch + "` prints 0: PSTACK-SYNC LAND: NOTHING\n"
        + "2. `git -C " + main + " status --porcelain` prints anything, `git -C " + main + " branch --show-current` is not main, or `git -C " + main + " status` reports a merge, rebase, cherry-pick, revert, or bisect in progress: PSTACK-SYNC LAND: WAIT\n"
        + "3. `git merge-base --is-ancestor main " + a.branch + "` fails: PSTACK-SYNC LAND: STALE\n"
        + "4. Otherwise run `git -C " + main + " merge --ff-only " + a.branch + "`. When it succeeds: PSTACK-SYNC LAND: DONE. When it fails: PSTACK-SYNC LAND: FAILED, then the error on the same line.\n"
        + "Put the line PSTACK-SYNC REVIEW just before the marker when target/pstack-sync/" + run + ".md lists anything for a person. Never push. Never commit, stash, reset, or resolve anything in " + main + ": conflicts are resolved on " + a.branch + " only, in a later sync step."
}
