// Chart labels for Sync: upstream's test/Sync.map.mjs, which chart-to-tla.mjs wrote from sync.mmd, in Rhai.
// Reader states are the chart's own states.

fn reader_actions() {
	[]
}

fn state(v) {
	`${v.connection} | ${v.syncBadge}`
}

fn step(action) {
	let labels = #{
		TheNetworkDrops: "the network drops",
		TheNetworkReturns: "the network returns",
		YouEditANote: "you edit a note",
		TheServerConfirms: "the server confirms",
	};
	labels[action] ?? action
}
