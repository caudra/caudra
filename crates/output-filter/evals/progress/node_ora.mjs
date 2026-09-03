// An ora spinner, the most used Node spinner.
//
// Measured rather than assumed: ora gates on `stream.isTTY`, so on a pipe it
// prints the opening frame and the final one and never redraws between them.
import ora from "ora";

const spinner = ora("Loading weights").start();
let done = 0;
const timer = setInterval(() => {
  spinner.text = `Loading weights ${++done}/200`;
  if (done >= 200) {
    clearInterval(timer);
    spinner.succeed("done");
  }
}, 3);
