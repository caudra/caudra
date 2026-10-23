---- MODULE SaveCode ----
VARIABLES screen, note, retries
vars == <<screen, note, retries>>
Init == screen = "Editing" /\ note = [text |-> "draft", rev |-> 0] /\ retries = 0
Save == screen = "Editing" /\ screen' = "Saving" /\ UNCHANGED <<note, retries>>
Confirm == screen = "Saving" /\ screen' = "Saved" /\ note' = [text |-> "stored", rev |-> 1] /\ UNCHANGED retries
Edit == screen = "Saved" /\ screen' = "Editing" /\ note' = [note EXCEPT !.text = "draft"] /\ retries' = 0
Retry == screen = "Saving" /\ retries = 0 /\ retries' = 1 /\ UNCHANGED <<screen, note>>
TimeOut == screen = "Saving" /\ retries = 1 /\ screen' = "Saved" /\ UNCHANGED <<note, retries>>
Next == Save \/ Retry \/ Confirm \/ TimeOut \/ Edit
Spec == Init /\ [][Next]_vars
TypeOK == screen \in {"Editing", "Saving", "Saved"}
\* You never see "Saved" while the note is still a draft.
NoFalseSaved == ~(screen = "Saved" /\ note.text = "draft")
====
