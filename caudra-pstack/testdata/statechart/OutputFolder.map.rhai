// Chart labels for OutputFolder: upstream's OutputFolder.map.mjs in Rhai.

fn reader_actions() {
	["Start", "Edit", "Note", "Stop", "KillNode"]
}

fn going(v) {
	let quiet = ["idle", "exited", "stopped", "orphanTlc"];
	v.pc.filter(|p| !quiet.contains(p)).len()
}

fn changed(before, after) {
	for i in 0..after.pc.len() {
		if after.pc[i] != before.pc[i] || after.aside[i].exists != before.aside[i].exists {
			return i + 1;
		}
	}
	0
}

fn context(v) {
	if v.pc.all(|p| p == "idle") {
		return "Before any run";
	}
	let runs = going(v);
	if runs == 2 {
		"Two runs going"
	} else if runs == 1 {
		"One run going"
	} else {
		"Done"
	}
}

fn specs(read) {
	switch read.map(|run| run.to_string()).reduce(|all, run| if all == () { run } else { `${all},${run}` }) {
		"1" => "the spec",
		"2" => "the edited spec",
		"1,2" => "both specs",
		_ => "",
	}
}

fn contents(v) {
	let kept = v.aside.filter(|a| a.exists);
	let aside = if kept.some(|a| a.notes) {
		", your notes moved aside"
	} else if kept.some(|a| a.by != 0) {
		", the old chart moved aside"
	} else {
		""
	};
	if !v.out.exists {
		return `no folder${aside}`;
	}
	let parts = [];
	if v.out.by != 0 {
		parts.push(`chart of ${specs(v.out.read)}`);
	}
	if v.out.notes {
		parts.push("your notes");
	}
	let words = if parts.is_empty() { "empty folder" } else { parts.reduce(|all, part| if all == () { part } else { `${all} and ${part}` }) };
	`${words}${aside}`
}

fn state(v) {
	`${context(v)}: ${contents(v)}`
}

fn group(v) {
	if v.pc.all(|p| p == "idle") {
		return "Not started";
	}
	if going(v) > 0 { "Running" } else { "Finished" }
}

fn step(action, before, after) {
	let r = changed(before, after);
	switch action {
		"Start" => {
			let where = if going(before) > 0 { " in a second shell" } else { "" };
			let how = if after.inputOk[r - 1] { "" } else { " with a typo" };
			`you start run ${r}${where}${how}`
		}
		"Refuse" => if before.inputOk[r - 1] {
			`run ${r} says the folder has a file that it did not write`
		} else {
			`run ${r} says the input is wrong`
		},
		"Tlc" => `run ${r}'s TLC reads the spec`,
		"Draw" => `run ${r} draws the chart in its own folder`,
		"Stale" => `run ${r} says the spec changed while it read it`,
		"Foreign" => `run ${r} says the folder has a file that it did not write`,
		"Swap" | "Checked" => "",
		"MoveIn" => `run ${r} moves its folder into place`,
		"MoveAside" => `run ${r} moves the old folder aside`,
		"Restore" => `run ${r} puts the folder with your notes back and stops`,
		"Supersede" => `run ${r} finds another run's folder in place and deletes the old folder`,
		"KeepAside" => `run ${r} keeps the folder with your notes aside and says where`,
		"RenameFails" => if after.aside[r - 1].exists {
			`run ${r} cannot rename a folder and says where the old folder is`
		} else if after.out.exists && !before.out.exists {
			`run ${r} cannot rename a folder, puts the old folder back, and stops`
		} else {
			`run ${r} cannot rename a folder and stops`
		},
		"Clean" => `run ${r} deletes the old folder and finishes`,
		"Edit" => "you edit the spec",
		"Note" => "you save notes in the folder",
		"Stop" => `you stop run ${r} with Ctrl-C`,
		"KillNode" => `you kill run ${r}'s Node, and its TLC keeps running`,
		"OrphanEnd" => `run ${r}'s orphaned TLC finishes`,
		_ => action,
	}
}

fn cause(v, facts, invariant) {
	let causes = #{
		ChartsOneSpec: "the run read the spec before and after an edit, and still wrote its chart",
		KeepsYourFiles: "a run deleted a folder that had your notes in it",
		NeverLosesChart: "a run deleted the last chart before another chart was in place",
		ExitTwoLeavesFolder: "a run wrote the folder and then said it could not finish",
		OverlapNeverRefuses: "a run stopped only because another run overlapped it",
	};
	causes[invariant]
}
