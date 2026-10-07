let meta = #{
    name: "status-desk",
    description: "Answer status questions from other sessions without a model turn, and pass every other direct message to the model",
    triggers: [#{ kind: "message_received", audiences: ["direct"], consume: true }],
    limits: #{ max_per_hour: 30 },
    messaging: #{ reply: true },
};
if event.sender == () { release("a script cannot take a reply"); }
let question = event.text.to_lower();
question.trim();
if !(question in ["status", "status?"]) { release("not a status question"); }
let s = event.session;
let goal = s.goal?.condition ?? "none";
reply(s.name + " (" + s.title + ") is " + s.status + ". Goal: " + goal + ".");
