# Looking at rendered UI

Emil's skills judge UI by how it looks and moves. Caudra runs in a terminal and has no browser of its own, so a visual check uses what the project and the session already have. Never claim to have seen something you did not render.

## In Build mode

1. Start the project's dev server with `shell` and `background: true`, using the script the project defines (`npm run dev`, `pnpm dev`, `bun dev`). Wait for the ready line in its output, not for the admission receipt.
2. Capture with tooling that is already there:
   - a browser-automation or screenshot MCP server, found through `code` (`help()` lists the servers, `help("<server>")` their tools). Never invent a tool name.
   - the project's own Playwright, Puppeteer, or Storybook test runner, run through `shell`.

   Never install a browser or a dependency for this. Write screenshots to the scratch directory (`$TMPDIR` in shell commands).
3. Look at each PNG with `view_image`. Compare states side by side by viewing both.
4. Stop the dev server when you are done, unless the user wants it kept.

Screenshots or recordings the user attaches or names count too: view the images with `view_image`.

## Without capture tooling, or in Plan mode

Reason from the code: the CSS, the layout primitives, the motion values. Label each finding **inferred from code** rather than **seen**. Then ask the user to look, naming the exact state, width, and interaction to check, through `question` when it is available with an answer channel.

## What a still image cannot show

`view_image` reads a frame. Easing, timing, interruption, dropped frames, and gesture feel need motion. For those, give the user the check to run (slow the animation down in DevTools, record the screen, try it on a real phone) and report the result as unverified until they say what they saw.
