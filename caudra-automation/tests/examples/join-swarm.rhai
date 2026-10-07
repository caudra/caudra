let meta = #{
    name: "join-swarm",
    description: "Announce this worker on swarm.status when it starts or resumes",
    triggers: [#{ kind: "armed" }],
    messaging: #{ publish: ["swarm.status"] },
};
if !(event.reason in ["launch", "resume"]) { skip("armed by hand"); }
publish("swarm.status", event.session.name + " is online");
