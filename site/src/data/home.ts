export const TAGLINE = 'Context into effective action';
export const HEADLINE = 'A coding agent that turns smart context into effective action.';
export const HERO_EYEBROW = 'Open source, for your terminal';
export const SOCIAL_CARD_ALT = `Caudra. ${HERO_EYEBROW}. ${HEADLINE}`;
export const METHOD_NOTE = "These are the maintainer's own measurements from daily use, not benchmarks. Timings were measured by hand. Results depend on models, projects, and hardware.";
export const CLAUDE_NOTE = "Claude subscription sign-in is experimental. Anthropic's terms limit Pro and Max subscriptions to official clients.";
export const WORKCELL_REPOSITORY = 'https://github.com/tensorninja/workcell-mcp';

export interface Link {
  name: string;
  href: string;
}

export interface Point {
  title: string;
  text: string;
}

export interface Segment {
  start: number;
  end: number;
  kind: 'write' | 'run';
}

export interface Lane {
  label: string;
  segments: readonly Segment[];
  marker?: { at: number; text: string };
}

export interface Step {
  mark: string;
  title: string;
  text: string;
}

export type Diagram =
  | { kind: 'lanes'; description: string; lanes: readonly Lane[] }
  | { kind: 'steps'; steps: readonly Step[] }
  | { kind: 'layout'; description: string; lines: readonly string[] };

export interface Clip {
  id: string;
  title: string;
  label: string;
  summary: string;
  diagram: Diagram;
}

export interface Story {
  id: string;
  index: string;
  eyebrow: string;
  title: readonly string[];
  lede: string;
  tone: 'light' | 'mist' | 'dark' | 'signal';
  layout: 'stacked' | 'split' | 'split-reverse';
  points: readonly Point[];
  note?: string;
  credits?: { label: string; links: readonly Link[] };
  link: { href: string; label: string };
  clips: readonly Clip[];
}

export interface Capability {
  name: string;
  href: string;
  experimental?: boolean;
}

export interface CapabilityGroup {
  name: string;
  items: readonly Capability[];
}

export interface Metric {
  value: string;
  label: string;
}

export interface Experiment {
  name: string;
  text: string;
  href: string;
}

const RTK: Link = { name: 'RTK', href: 'https://www.rtk-ai.app/docs/' };
const RIPWIRE: Link = { name: 'ripwire', href: 'https://github.com/redhat-et/ripwire' };
const PLANNOTATOR: Link = { name: 'Plannotator', href: 'https://plannotator.ai/' };
const HERDR: Link = { name: 'Herdr', href: 'https://herdr.dev' };
export const GROK_BUILD: Link = { name: 'Grok Build workflows', href: 'https://x.ai/news/workflows' };
export const NINFER: Link = { name: 'ninfer-4090', href: 'https://github.com/tensorninja/ninfer-4090' };
const GRAHN_2008: Link = { name: 'Grahn, Parkinson and Owen, 2008', href: 'https://www.sciencedirect.com/science/article/abs/pii/S0301008208001019' };
const LAU_2007: Link = { name: 'Lau and Glimcher, 2007', href: 'https://www.jneurosci.org/content/27/52/14502' };
const DOI_2020: Link = { name: 'Doi et al., 2020', href: 'https://elifesciences.org/articles/56694' };
export const NAME_SOURCES = [GRAHN_2008, LAU_2007, DOI_2020];

const linked = ({ name, href }: Link) => `[${name}](${href})`;

export const inspirations = [
  { ...RTK, idea: 'shell output filtering' },
  { ...RIPWIRE, idea: 'code maps' },
  { ...PLANNOTATOR, idea: 'passage review' },
  { ...HERDR, idea: 'terminal workspaces for agents' },
] as const;

export const metrics: readonly Metric[] = [
  { value: '20B+', label: 'tokens a month through Caudra' },
  { value: '<2 GB', label: 'for over a month of complete local history' },
  { value: '<1 s', label: 'to load or save a 10k-turn session, subagents included' },
  { value: '~97%', label: 'Anthropic prompt-cache hit rate, and about 94% on OpenAI' },
];

export const heroClip: Clip = {
  id: 'eager',
  title: 'Work starts while the model writes',
  label: 'Eager execution',
  summary: 'The order in which work starts during one streamed response. Bars show order, not measured time.',
  diagram: {
    kind: 'lanes',
    description: 'The model response streams from start to end. A file_grep call is drawn as it is written and runs once its arguments are complete, followed by a file_read call. A task chat opens while its brief is still being written, and the subagent starts when the brief is complete.',
    lanes: [
      { label: 'Response', segments: [{ start: 0, end: 100, kind: 'write' }] },
      { label: '`file_grep`', segments: [{ start: 4, end: 18, kind: 'write' }, { start: 18, end: 50, kind: 'run' }] },
      { label: '`file_read`', segments: [{ start: 22, end: 32, kind: 'write' }, { start: 32, end: 58, kind: 'run' }] },
      { label: '`task`', segments: [{ start: 40, end: 72, kind: 'write' }, { start: 72, end: 100, kind: 'run' }], marker: { at: 41, text: 'Chat opens' } },
    ],
  },
};

export const nameStory = {
  title: ['Caudra comes', 'from caudate.'],
  paragraphs: [
    'Caudra, pronounced KAW-druh, is named after the caudate nucleus. This part of the brain belongs to the circuits that connect evidence and goals to action. Studies link it to learning which actions lead to which outcomes.',
    'The name describes how Caudra works. It turns the context of your task into an action, then reads the outcome before it chooses the next one.',
    'The brain is only the inspiration for the name. Caudra is software, and it works toward the goal you set.',
  ],
  loop: ['Context and intent', 'Evaluate evidence', 'Select an action', 'Execute with tools', 'Observe the outcome'],
  loopNote: 'Each outcome becomes evidence for the next step.',
  sources: `Sources: ${NAME_SOURCES.map(linked).join(', ')}.`,
};

export const stories: readonly Story[] = [
  {
    id: 'steer',
    index: '01',
    eyebrow: 'Steer anything, any time',
    title: ['Guide any agent', 'while it works.'],
    lede: 'The input stays open while Caudra works. Queue the next task, add guidance to the current run, or replace it. Open any running subagent and guide it directly.',
    tone: 'mist',
    layout: 'split',
    points: [
      { title: 'Next, Guide, and Replace', text: '`Enter` queues a prompt for after the current run. `Ctrl+X g` adds guidance before the next model request, and `Ctrl+X x` stops the run and starts yours. Queued prompts stay editable.' },
      { title: 'Guide a subagent directly', text: 'Open a running subagent from `/tasks` and type. Your guidance stays visible until the subagent reads it at its next turn boundary.' },
      { title: 'Read a brief as it is written', text: "A subagent's chat opens while the model is still writing its instructions, so you can read the task before it starts." },
      { title: 'Ask `/btw` on the side', text: 'Ask about the conversation without adding to its history. When the main agent asks you a question, `F2` opens `/btw` to clarify the choices, and `Esc` returns to the unchanged form.' },
    ],
    link: { href: '/docs/queue/', label: 'Queue and steering' },
    clips: [
      {
        id: 'steer',
        title: 'Guide a running subagent',
        label: 'Guide a subagent',
        summary: 'Where your input goes while work runs.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: 'Enter', title: 'Next', text: 'Waits for the current run and its background work.' },
            { mark: 'Ctrl+X g', title: 'Guide', text: 'Joins the run before its next model request.' },
            { mark: 'Ctrl+X x', title: 'Replace', text: 'Stops the run and starts your instruction.' },
            { mark: '/tasks', title: 'Guide a subagent', text: 'Reaches that subagent at its next turn boundary.' },
          ],
        },
      },
      {
        id: 'btw',
        title: 'Clarify a question with /btw',
        label: 'Ask /btw',
        summary: 'A side question while the agent waits for your answer.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: '?', title: 'The agent asks', text: 'A question form waits for your choice.' },
            { mark: 'F2', title: 'Ask `/btw`', text: 'Clarify the choices in a side thread.' },
            { mark: 'Esc', title: 'Return', text: 'The form is unchanged, and the side thread stays out of history.' },
          ],
        },
      },
    ],
  },
  {
    id: 'sleep',
    index: '02',
    eyebrow: 'While you sleep',
    title: ['Set the finish line.', 'Wake to results.'],
    lede: '`/goal` keeps a session working until a separate evaluator finds evidence that your condition is met. Background work reports back without polling, and your permission rules decide what runs while you are away.',
    tone: 'dark',
    layout: 'split-reverse',
    points: [
      { title: 'Goals that check evidence', text: 'After each work turn, a separate model call with no tools reads the transcript. An unmet goal starts another turn. A met goal clears itself.' },
      { title: 'Background work wakes the agent', text: 'Tasks and shell jobs can keep running in the background. Their reports start the next step at a safe boundary, with no polling.' },
      { title: 'Permissions that read the command', text: 'Shell chains and pipelines split into separate commands for approval. Approve once, for the conversation, the project, or every project, and accept patterns Caudra suggests from repeated approvals.' },
      { title: 'Plan first, then hear back', text: 'Caudra opens in Plan mode, where the agent edits only its plan and asks before commands it cannot prove read-only. Notifications tell you when a turn finishes or a prompt needs your answer.' },
    ],
    note: 'Unattended work needs a running session, for example in tmux or Herdr. Closing it cancels background work. A goal allows 16 automatic continuations by default, and work pauses when it needs your answer. `/goal` looks for evidence in the transcript. It does not prove the work is correct.',
    link: { href: '/docs/commands/#completion-goals', label: 'Completion goals' },
    clips: [
      {
        id: 'goal',
        title: 'Work toward a goal',
        label: 'Completion goal',
        summary: 'The loop that /goal runs after each work turn.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: '/goal', title: 'Set the condition', text: 'For example: tests pass and clippy is clean.' },
            { mark: '1', title: 'Work', text: 'The turn ends and background results settle.' },
            { mark: '2', title: 'Evaluate', text: 'A separate call looks for evidence in the transcript.' },
            { mark: '↺', title: 'Continue', text: 'An unmet goal starts another turn, up to the limit.' },
            { mark: '✓', title: 'Done', text: 'A met goal clears itself.' },
          ],
        },
      },
    ],
  },
  {
    id: 'savings',
    index: '03',
    eyebrow: 'Built-in token savings',
    title: ['Token savings,', 'built in.'],
    lede: 'Every turn re-sends the conversation, so a noisy result costs tokens again on every later turn until compaction. Caudra keeps results small and round-trips few, starting with RTK-style shell output filtering that is on by default.',
    tone: 'signal',
    layout: 'stacked',
    points: [
      { title: 'RTK-style output filtering', text: 'Command-aware rules trim build and test noise from completed shell output before the model reads it. You watch raw output while the command runs and can switch between filtered and raw views.' },
      { title: 'Structure before content', text: '`file_index` returns a file outline with signatures and line numbers. The `code_*` tools rank symbols and show callers, impact, and the tests that reach a change. They parse the source on the fly, so there is no index to build or maintain and nothing to configure.' },
      { title: 'Fewer, smaller turns', text: 'Oversized results reach the model as a bounded head and tail, and `tool_output` searches the rest. `batch` runs independent calls in one turn, and subagents keep their exploration out of the main context.' },
      { title: 'Requests shaped for caching', text: 'Caudra sends a per-conversation cache key where providers accept one, and `/usage` scores the cache hit rate for every model.' },
    ],
    note: "In the maintainer's daily use, about 97% of Anthropic and 94% of OpenAI prompt tokens came from cache.",
    credits: { label: 'With credit to the ideas in', links: [RTK, RIPWIRE] },
    link: { href: '/docs/token-economy/', label: 'How Caudra saves tokens' },
    clips: [
      {
        id: 'filter',
        title: 'Filter shell output',
        label: 'Output filtering',
        summary: 'How shell output is filtered before the model reads it.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: '$', title: 'Run', text: 'You watch the raw output while the command runs.' },
            { mark: 'filter', title: 'Trim', text: 'Command-aware rules remove routine noise from the completed output.' },
            { mark: '→', title: 'Read', text: 'The model receives the filtered result.' },
            { mark: 'raw', title: 'Compare', text: 'Switch between filtered and raw views in the transcript.' },
          ],
        },
      },
    ],
  },
  {
    id: 'workbench',
    index: '04',
    eyebrow: 'A workbench beside the agent',
    title: ['Open the code', 'without leaving.'],
    lede: '`Ctrl+X w` brings up a file explorer, tabbed editor, project search, and source control while the session keeps running. It lives in the terminal, so it comes along over SSH.',
    tone: 'light',
    layout: 'split',
    points: [
      { title: 'Read the actual change', text: 'Inspect diffs, browse the commit graph, and stage, unstage, or discard per file or folder.' },
      { title: 'Point at exact lines', text: '`Ctrl+X Enter` sends the file, line, or selection to the composer as a mention such as `@src/api.ts:L10-L20`. Caudra puts those lines in the request.' },
      { title: 'Review passages of a reply', text: 'Mark the parts of an answer that need work, add a note to each, and send every note back as one prompt.' },
      { title: 'Replies rendered for engineers', text: 'Tables, highlighted code, Unicode maths, and Mermaid flowcharts render in the terminal. Copying a passage gives back its Markdown source.' },
    ],
    credits: { label: 'Passage review was inspired by', links: [PLANNOTATOR] },
    link: { href: '/docs/workbench/', label: 'Explore the workbench' },
    clips: [
      {
        id: 'workbench',
        title: 'The workbench beside a session',
        label: 'Workbench',
        summary: 'The workbench layout, taken from its documentation.',
        diagram: {
          kind: 'layout',
          description: 'Workbench layout: a sidebar with files, Git, and search views on the left, editor tabs and a buffer on the right, and a status row that offers Ctrl+X Enter to send a reference.',
          lines: [
            '┌─────────────────┬───────────────────────────────┐',
            '│ FILES GIT FIND  │  a.txt ×  ●b.rs ×             │',
            '│  sub/           │  1  one                       │',
            '│  a.txt        M │  2  two                       │',
            '│  b.rs         U │  3  three                     │',
            '├─────────────────┴───────────────────────────────┤',
            '│ a.txt        Ln 3, Col 1  LF  Ctrl+X Enter send │',
            '└─────────────────────────────────────────────────┘',
          ],
        },
      },
      {
        id: 'review',
        title: 'Review passages of a reply',
        label: 'Passage review',
        summary: 'The passage review keys.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: 'Ctrl+X r', title: 'Open', text: 'Review the last reply, or any message from its menu.' },
            { mark: 'Shift+↓', title: 'Select', text: 'Take the rows that need work, or drag across them.' },
            { mark: 'Enter', title: 'Note', text: 'Write a note on the selection.' },
            { mark: 'Ctrl+S', title: 'Send', text: 'Every note lands in the prompt as one block.' },
          ],
        },
      },
    ],
  },
  {
    id: 'thread',
    index: '05',
    eyebrow: 'Never lose the thread',
    title: ['Go back to any', 'point in the work.'],
    lede: 'Every session keeps its full history, subagent transcripts included, in compact local storage. Revert the conversation, the files, or both from a message, and preview the file changes first.',
    tone: 'mist',
    layout: 'split-reverse',
    points: [
      { title: 'Large sessions open fast', text: "Payloads are compressed, appending saves write only new rows, and the transcript lays out only what is on screen. In the maintainer's use, a 10k-turn session loads in under a second." },
      { title: 'Revert with a preview', text: 'Each tool call that may change files is recorded before and after it runs, so a revert touches only those files. The first Revert files shows what would change, and repeating it applies the revert.' },
      { title: 'Memory that outlasts the session', text: 'The agent keeps tagged notes on project gotchas and decisions outside your repository. Requests carry only the tags until a note is needed, and `/memory` lets you read, edit, or delete them.' },
      { title: 'Fork and branch out', text: 'Fork from a message to try another approach. `/worktree new` moves a session into a fresh Git worktree with its conversation and plan.' },
    ],
    note: 'File revert cannot undo external side effects such as running processes, databases, network calls, or Git branch state.',
    credits: { label: 'Worktrees and agent status integrate with', links: [HERDR] },
    link: { href: '/docs/sessions/', label: 'Sessions and revert' },
    clips: [
      {
        id: 'revert',
        title: 'Revert files from a message',
        label: 'Revert',
        summary: 'A file revert with its preview.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: '⋮', title: 'Open the menu', text: 'Beside any message in the main transcript.' },
            { mark: '1', title: 'Preview', text: 'Revert files counts what would be created, replaced, or deleted.' },
            { mark: '2', title: 'Apply', text: 'Repeat the action. Any conflict aborts the whole revert.' },
            { mark: '↶', title: 'Unrevert', text: 'Put the files and the conversation head back.' },
          ],
        },
      },
    ],
  },
  {
    id: 'nudges',
    index: '06',
    eyebrow: 'Automatic steering',
    title: ['Keep any model', 'on task.'],
    lede: 'Smaller local models such as Qwen3.8-27B need more help to finish a task. Caudra nudges the model when a turn stalls, gets cut off, loops, or stops after announcing work. The rules react to what a reply did, so the same defaults work well with flagship models.',
    tone: 'light',
    layout: 'split',
    points: [
      { title: 'Nudges for stalled turns', text: 'After an empty or cut-off reply, Caudra asks the model to continue. After “I will run the tests now” with no tool call, it asks for the work itself. The third identical tool call in a row is refused before it runs.' },
      { title: 'Hints when work goes in circles', text: 'When the model repeats a tool cycle or an answer, or keeps making failed calls, a hint asks it to reconsider its approach. Hints stop at four per run by default and never reopen a finished answer.' },
      { title: 'Sensible defaults, tuned per model', text: 'Every rule is on by default, and budgets cap how often each one fires. Change a threshold, a budget, or the wording of a nudge for all models, or only for one exact `provider/model-id`.' },
      { title: 'Every nudge in the transcript', text: 'Each nudge appears as a dim row. Click it to read the exact text the model received.' },
    ],
    note: 'A nudge is a message to the model. It cannot run or approve a tool, and every real tool call still passes through validation and your permission rules.',
    link: { href: '/docs/configuration/#agent-steering', label: 'Configure automatic steering' },
    clips: [
      {
        id: 'nudge',
        title: 'Nudge a turn that stopped early',
        label: 'Nudge',
        summary: 'What happens when a reply announces work and calls no tool.',
        diagram: {
          kind: 'steps',
          steps: [
            { mark: '1', title: 'Stop early', text: 'The reply ends on “I will run the tests now” and calls no tool.' },
            { mark: '2', title: 'Nudge', text: 'The `abandoned_turn` rule asks the model to do that work now.' },
            { mark: '3', title: 'Read the nudge', text: 'A dim row in the transcript holds the exact text the model received.' },
            { mark: '↺', title: 'Continue', text: 'The next reply can call its tools. By default, a third announcement in a row ends the turn as written.' },
          ],
        },
      },
    ],
  },
];

export const signIns = [
  { name: 'ChatGPT', text: 'Sign in with your ChatGPT subscription.', experimental: false },
  { name: 'GitHub Copilot', text: 'Reuse an existing Copilot sign-in.', experimental: false },
  { name: 'xAI', text: 'Sign in with your xAI account.', experimental: false },
  { name: 'Claude', text: 'Sign in with your Claude subscription.', experimental: true },
] as const;

export const providerNames = ['Anthropic', 'OpenAI', 'Google', 'GitHub Copilot', 'xAI', 'Mistral', 'DeepSeek', 'OpenRouter', 'Z.AI', 'Ollama', 'llama.cpp'] as const;

export const modelJobsIntro = 'Nine jobs decide which model serves each kind of work. Pin a job to a model in `/model`, or let it follow Chat, Plan, Fast, or Best. A local endpoint can name its own Fast and Best models in `providers.toml`.';

export const modelJobs: readonly Point[] = [
  { title: 'Chat', text: 'The main conversation. Picking a model in `/model` sets it.' },
  { title: 'Plan', text: 'Main turns in Plan mode, on the Chat model until you bind it.' },
  { title: 'Subagent', text: "Delegated tasks, on the parent agent's model until you bind it." },
  { title: 'Compact', text: 'Summaries when a context window fills.' },
  { title: 'Title', text: 'Session names, on Fast until you bind it.' },
  { title: 'Goal', text: 'The `/goal` evaluator, on Fast until you bind it.' },
  { title: 'Extract', text: 'Requirements for `/extract` and compaction, on Fast until you bind it.' },
  { title: 'Fast', text: 'The preferred small model, for routine calls.' },
  { title: 'Best', text: "The provider's flagship. Point Plan or a prompt profile's subagents at it." },
];

export const capabilityGroups: readonly CapabilityGroup[] = [
  {
    name: 'Agents',
    items: [
      { name: 'Steerable subagents', href: '/docs/commands/#tasks' },
      { name: 'Background tasks and shell jobs', href: '/docs/sessions/#background-tasks' },
      { name: 'Next, Guide, and Replace queue', href: '/docs/queue/' },
      { name: 'Completion goals with `/goal`', href: '/docs/commands/#completion-goals' },
      { name: 'Side questions with `/btw`', href: '/docs/commands/#modes-and-toggles' },
      { name: 'A model for each job', href: '/docs/providers/#model-jobs' },
      { name: 'Automatic steering, tuned per model', href: '/docs/configuration/#agent-steering' },
      { name: 'Requirements tracking with `/extract`', href: '/docs/commands/#requirements' },
      { name: 'Resume a stopped turn with `/continue`', href: '/docs/commands/#resuming-after-an-interruption' },
      { name: 'Durable workflows', href: '/docs/workflows/', experimental: true },
    ],
  },
  {
    name: 'Tools',
    items: [
      { name: 'File read, search, edit, and patch', href: '/docs/tools/#file-operations' },
      { name: 'File outlines with `file_index`', href: '/docs/tools/#file_index' },
      { name: 'Code maps with no index to maintain', href: '/docs/tools/#code-intelligence' },
      { name: 'Shell with output filtering', href: '/docs/tools/#shell' },
      { name: 'Isolated Python for computation', href: '/docs/tools/#python_execution' },
      { name: 'Parallel calls with `batch`', href: '/docs/tools/#batch' },
      { name: 'Retained output search', href: '/docs/tools/#tool_output' },
      { name: 'Web search and fetch, PDFs included', href: '/docs/tools/#web' },
      { name: 'Image generation with a ChatGPT login', href: '/docs/tools/#image_generate' },
      { name: 'Tool-call JSON repair', href: '/docs/token-economy/#fewer-round-trips' },
      { name: 'Tools loaded on demand', href: '/docs/tools/#tools-loaded-on-demand' },
    ],
  },
  {
    name: 'Context',
    items: [
      { name: 'Instruction files with `AGENTS.md`', href: '/docs/context/#instruction-files' },
      { name: 'Project memory across sessions', href: '/docs/context/#four-places-to-put-knowledge' },
      { name: 'Skills from `.claude` and `.agents` folders', href: '/docs/skills/#where-skills-live' },
      { name: '`@file` and `#commit` mentions', href: '/docs/context/#mention-a-file-with' },
      { name: 'Inspect with `/context` and `/projection`', href: '/docs/context/#inspect-the-active-window' },
      { name: 'Compaction that keeps recent turns', href: '/docs/context/#when-the-window-fills' },
      { name: 'System prompt profiles', href: '/docs/system-prompts/' },
      { name: 'The full manual offline in `/docs`', href: '/docs/commands/#docs' },
    ],
  },
  {
    name: 'Sessions',
    items: [
      { name: 'Revert conversation, files, or both', href: '/docs/sessions/#message-actions' },
      { name: 'File changes recorded per call', href: '/docs/sessions/#recording' },
      { name: 'Forks from any message', href: '/docs/sessions/#fork-boundaries' },
      { name: 'Git worktrees', href: '/docs/worktrees/' },
      { name: 'Retention policies and `/storage`', href: '/docs/sessions/#retention' },
      { name: 'Move sessions between directories', href: '/docs/sessions/#moving-sessions-to-another-directory' },
      { name: 'Spend and cache hit rate in `/usage`', href: '/docs/token-economy/#cache-hit-rate' },
    ],
  },
  {
    name: 'Interface',
    items: [
      { name: 'Workbench with Git and search', href: '/docs/workbench/' },
      { name: 'Passage review', href: '/docs/review/' },
      { name: 'Markdown with maths and Mermaid', href: '/docs/markdown/' },
      { name: 'Tool calls drawn as they stream', href: '/docs/token-economy/#fewer-round-trips' },
      { name: 'Draft stash and command palette', href: '/docs/commands/#stash' },
      { name: 'Paired light and dark themes', href: '/docs/configuration/#ui-theme' },
      { name: 'Notifications in tmux and Herdr', href: '/docs/notifications/' },
    ],
  },
  {
    name: 'Safety',
    items: [
      { name: 'Parsed shell permissions', href: '/docs/permissions/#shell-parsing' },
      { name: 'Scoped approvals', href: '/docs/permissions/#per-command-scopes' },
      { name: 'Patterns suggested from history', href: '/docs/permissions/#suggested-patterns' },
      { name: 'Plan mode', href: '/docs/permissions/#plan-mode' },
      { name: 'No tracking, opt-in telemetry', href: '/docs/telemetry/' },
      { name: 'Managed VM sandboxes', href: '/docs/sandboxes/', experimental: true },
      { name: 'JEV decision engine', href: '/docs/configuration/#decisions', experimental: true },
    ],
  },
  {
    name: 'Integrations',
    items: [
      { name: 'MCP servers with tool search', href: '/docs/mcp/#tool-search' },
      { name: 'ACP for Zed', href: '/docs/acp/' },
      { name: 'Headless runs for scripts and CI', href: '/docs/headless/' },
      { name: 'Claude Code compatible output', href: '/docs/headless/#claude-code-compatibility' },
      { name: 'Custom slash commands', href: '/docs/commands/#custom-commands' },
      { name: 'Herdr integration', href: '/docs/worktrees/#herdr-integration' },
      { name: 'Workcell tools, also a standalone MCP server', href: WORKCELL_REPOSITORY },
      { name: 'Cross-session messaging', href: '/docs/messaging/', experimental: true },
      { name: 'Lua extensions', href: '/docs/plugins/', experimental: true },
      { name: 'Remote Workcell workspaces', href: '/docs/remote-workspaces/', experimental: true },
    ],
  },
];

export const capabilityCount = capabilityGroups.reduce((total, group) => total + group.items.length, 0);

export const sandbox = {
  title: ['Move execution', 'into a VM.'],
  text: 'Managed sandboxes run workspace tools in a separate VM. Model connections, provider credentials, and the conversation stay on your machine, so a compromised VM cannot read credentials it never received.',
  points: [
    'Network policy allows listed domains and IP ranges, with TLS hostname checks or an inspecting proxy.',
    'File transfers between your machine and the VM go through a review.',
    'Requires e2b-libvirt infrastructure that you or your operator run.',
  ],
  note: 'Transferred files can still carry secrets. The separation reduces exposure and is no guarantee against compromise.',
  href: '/docs/sandboxes/',
} as const;

export const experiments: readonly Experiment[] = [
  { name: 'Durable workflows', href: '/docs/workflows/', text: `Scripts launch subagents in phases, keep a journal, and can pause and resume. Each agent call can name a model job, such as Fast for a wide pass and Best for the answer you read. Built-in workflows cover deep research, change review, and root-cause analysis. They are heavily inspired by ${linked(GROK_BUILD)} and mostly compatible with them.` },
  { name: 'JEV decision engine', href: '/docs/configuration/#decisions', text: 'Typed decisions from an endpoint you configure: permission advice, Auto mode screening, shell effect and duration predictions, sampled web and MCP content screening, tool search ranking, skill suggestions, goal prescreening, and subagent routing to the Fast or Best model. Predictions add to deterministic permission rules and can miss risks.' },
  { name: 'Cross-session messaging', href: '/docs/messaging/', text: 'Live sessions on one machine exchange messages, publish to topics, and share work through consumer groups.' },
  { name: 'Lua extensions', href: '/docs/plugins/', text: 'Add your own commands, tools, and interface behavior in Lua when the built-in workflow needs something specific.' },
  { name: 'Remote Workcell', href: '/docs/remote-workspaces/', text: 'Run workspace tools on a Workcell server while the conversation stays on your machine.' },
];

export const privacyPoints: readonly Point[] = [
  { title: 'No tracking', text: 'Telemetry is off unless you send it to a collector you run.' },
  { title: 'Offline with a local model', text: `Use Ollama or llama.cpp without internet access, or point a \`providers.toml\` entry at ${linked(NINFER)}, the maintainer's custom inference engine for Qwen3.8-27B on one RTX\u00a04090. Otherwise a normal run contacts your provider, refreshes the public models.dev catalog at most once a day, and reaches Exa when the agent searches the web.` },
  { title: 'Updates on request', text: 'Caudra checks for updates only when you run `caudra update` or turn on the startup check.' },
];
