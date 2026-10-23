// Chart labels for Upload: upstream's test/Upload.map.mjs, which chart-to-tla.mjs wrote from upload.mmd, in Rhai.
// Reader states are the chart's own states.

fn reader_actions() {
	[]
}

fn state(v) {
	`${v.main}`
}

fn step(action) {
	let labels = #{
		TheLastByteArrives: "the last byte arrives",
		TheCheckPasses: "the check passes",
		YouChooseAFile: "you choose a file",
		TheNetworkDrops: "the network drops",
		YouRetry: "you retry",
		YouCancel: "you cancel",
	};
	labels[action] ?? action
}
