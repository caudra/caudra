---- MODULE SaveAgreed ----
VARIABLES screen, note
vars == <<screen, note>>
Init == screen = "Editing" /\ note = [text |-> "draft"]
Save == screen = "Editing" /\ screen' = "Saving" /\ UNCHANGED <<note>>
Confirm == screen = "Saving" /\ screen' = "Saved" /\ note' = [text |-> "stored"]
Edit == screen = "Saved" /\ screen' = "Editing" /\ note' = [note EXCEPT !.text = "draft"]
Next == Save \/ Confirm \/ Edit
Spec == Init /\ [][Next]_vars
TypeOK == screen \in {"Editing", "Saving", "Saved"}
\* You never see "Saved" while the note is still a draft.
NoFalseSaved == ~(screen = "Saved" /\ note.text = "draft")
====
