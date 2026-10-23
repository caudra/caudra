// Chart labels for SaveCode: upstream's test mapping for from-spec --against, in Rhai.

fn reader_actions() {
	["Save", "Edit"]
}

fn state(v) {
	`${v.screen}, ${v.note.text}`
}

fn step(action) {
	let labels = #{
		Save: "you press Save",
		Retry: "the app tries again",
		Confirm: "the server stores the note",
		TimeOut: "the save times out",
		Edit: "you edit the note",
	};
	labels[action] ?? ""
}

fn cause() {
	"the app stops after one retry and shows Saved anyway"
}
