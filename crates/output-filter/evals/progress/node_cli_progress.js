// A cli-progress bar, the most used Node progress library.
//
// Measured rather than assumed: cli-progress defaults `noTTYOutput` to false and
// so draws nothing when stdout is a pipe.
const cliProgress = require("cli-progress");

const bar = new cliProgress.SingleBar({}, cliProgress.Presets.shades_classic);
bar.start(200, 0);
let done = 0;
const timer = setInterval(() => {
  bar.update(++done);
  if (done >= 200) {
    clearInterval(timer);
    bar.stop();
  }
}, 3);
