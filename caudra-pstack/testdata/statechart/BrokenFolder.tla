---- MODULE BrokenFolder ----
EXTENDS OutputFolder

NoNotesAside == ~(\E r \in Runs : aside[r].notes)
NeverExitTwo == \A r \in Runs : exitCode[r] # "2"
NotBothWrote == ~(wrote[1] /\ wrote[2])
NoBroke == \A r \in Runs : ~broke[r]
====
