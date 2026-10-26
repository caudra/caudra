# Attribution

Caudra is an independent fork of [Maki](https://github.com/tontinton/maki), originally developed by [Tony Solomonik](https://github.com/tontinton), and includes work by Maki contributors.

Original Maki work is copyright (c) 2026 Tony Solomonik. Caudra modifications are copyright (c) 2026 [Thorsten Born](https://thorstenborn.com).

Caudra is independently maintained and is not affiliated with or endorsed by the original project.

## License transition

Caudra's first-party work is licensed under the Apache License, Version 2.0 from this transition forward. See [LICENSE](LICENSE) for the unmodified license text. Inherited MIT-licensed material and separately licensed third-party files retain their existing terms.

The Maki-derived code and Caudra work previously distributed under MIT remain available under those MIT terms. [THIRD_PARTY_LICENSES/Maki.txt](THIRD_PARTY_LICENSES/Maki.txt) preserves the previous root MIT license verbatim, including the Tony Solomonik and Thorsten Born copyright notices. This prospective transition does not replace earlier license grants or relicense historical contributions.

No Apache patent grant from the original Maki authors or other prior MIT contributors is asserted by this transition. Apache patent grants apply only as provided by that license from contributors granting rights under it.

## Third-party material

Third-party material remains under its own MIT, Apache-2.0, or other applicable license. Preserve the notices in [THIRD_PARTY_LICENSES/](THIRD_PARTY_LICENSES/) and any license and attribution notices accompanying individual files or vendored components. The first-party package license metadata does not override these terms.

The built-in `deep-research` workflow (`caudra-workflow/builtins/deep-research.rhai`) is adapted from Grok Build, copyright (c) 2023-2026 SpaceXAI, licensed under the Apache License, Version 2.0. The license text is in `THIRD_PARTY_LICENSES/GrokBuild.txt`. The workflow engine, journal, and runtime in `caudra-workflow` and `caudra-agent` are independent Caudra work that follows the Grok Build design.

The bundled pstack (`caudra-pstack/upstream/pstack/`) is copied from [cursor/plugins](https://github.com/cursor/plugins) at the revision recorded in `caudra-pstack/upstream/upstream.json`, copyright (c) 2026 Lauren Tan, licensed under the MIT License. See `THIRD_PARTY_LICENSES/Pstack.txt`. The `deslop`, `control-cli`, and `control-ui` skills (`caudra-pstack/upstream/cursor-team-kit/`) are copied from the same revision's cursor-team-kit, copyright (c) 2026 Cursor, licensed under the MIT License. See `THIRD_PARTY_LICENSES/CursorTeamKit.txt`. Both keep their license beside the copy. The only change to the copied files rewrites relative links to files left out of the copy as links to the upstream repository. `caudra-pstack/translated/` holds modified versions of some of these files, rewritten to name Caudra's tools, paths, and features in place of Cursor's, and they remain under the same MIT terms. The Rust ports of pstack's scripts in `caudra-pstack/src/scripts/` follow the upstream scripts' behaviour and remain under the same MIT terms.

The bundled mattpocock skills (`caudra-mattpocock/upstream/`) are copied from [mattpocock/skills](https://github.com/mattpocock/skills) at the revision recorded in `caudra-mattpocock/upstream/upstream.json`, copyright (c) 2026 Matt Pocock, licensed under the MIT License. See `THIRD_PARTY_LICENSES/MattPocockSkills.txt`. The license is kept beside the copy. The only change to the copied files rewrites relative links to files left out of the copy as links to the upstream repository. `caudra-mattpocock/translated/` holds modified versions of these files, rewritten to name Caudra's tools, paths, and features and adapted to how Caudra works, and they remain under the same MIT terms.

The bundled Monty interpreter is copyright (c) Pydantic Services Inc., licensed under the MIT License. See `THIRD_PARTY_LICENSES/Monty.txt`.

The statechart model checker is [tla-checker](https://github.com/fabracht/tla-rs), built from Caudra's fork at [tensorninja/tla-rs](https://github.com/tensorninja/tla-rs), copyright (c) 2026 Fabrício Bracht, licensed under the MIT License or the Apache License, Version 2.0, at your option. Caudra uses it under the MIT License. See `THIRD_PARTY_LICENSES/TlaChecker.txt`.

The file, shell, web, code, and Python tools are built on [Workcell](https://github.com/caudra/caudra/tree/main/workcell), copyright The Workcell MCP authors, licensed under the Apache License, Version 2.0. See `THIRD_PARTY_LICENSES/Workcell.txt`.

The Workcell shell output filter includes declarative filter rules copied without modification from [RTK](https://github.com/rtk-ai/rtk) at revision `4ef8730cf3a0dea11c8d64a9b90bc0616fe52100` (tag `v1.0.1`), copyright 2026 RTK AI Labs, licensed under the Apache License, Version 2.0. See `THIRD_PARTY_LICENSES/RTK.txt`.

The patched Crossterm terminal library retains its MIT license in `THIRD_PARTY_LICENSES/Crossterm.txt` and `vendor/crossterm/LICENSE`.

Workcell includes adapted Ripwire and tree-sitter queries. Their notices and recorded provenance are in `THIRD_PARTY_LICENSES/Ripwire.txt` and `THIRD_PARTY_LICENSES/TreeSitterQueries.txt`. The original Ripwire import revision is not recorded. The documented inspection revision is not an import provenance claim.

The bundled two-face syntax collection includes separately licensed syntax definitions. See `THIRD_PARTY_LICENSES/TwoFace.txt` and `THIRD_PARTY_LICENSES/TwoFaceSyntaxes.txt` for the library license and syntax acknowledgements.

The Rhai syntax definition (`caudra-highlight/syntaxes/rhai.sublime-syntax`) is adapted from [sublime-rhai](https://github.com/rhaiscript/sublime-rhai) at revision `3d973e99bb58df253f1e4b3c56baa776e2718451`, with local changes for Syntect compatibility and current Rhai syntax. The grammar, including these modifications, remains under the Mozilla Public License, Version 2.0. The upstream license is preserved verbatim in `THIRD_PARTY_LICENSES/SublimeRhai.txt`, and provenance is recorded in `THIRD_PARTY_LICENSES/additional/provenance.json`. Distribution attribution bundles include the corresponding modified grammar in the `caudra-highlight` source archive under `sources/` (inside `attribution.tar.gz` for compact bundles). This does not change the license of other Caudra files.

The TLA+, PlusCal, and TLC model configuration syntax definitions (`caudra-highlight/syntaxes/tlaplus.sublime-syntax`, `pluscal.sublime-syntax`, and `tlaplus-cfg.sublime-syntax`) are converted from the TextMate grammars in [vscode-tlaplus](https://github.com/tlaplus/vscode-tlaplus) (`languages/tlaplus-grammar.json`, `pluscal-grammar.json`, and `cfg-grammar.json`) at revision `2d54234dcb49b52e4b24e750c501e464e9c7aeb8`, with local changes for Syntect compatibility and broader TLA+ coverage, copyright (c) 2019 Andrew Lygin and (c) 2020 TLA+ Foundation, licensed under the MIT License. The upstream license is preserved verbatim in `THIRD_PARTY_LICENSES/VscodeTlaplus.txt`, and provenance is recorded in `THIRD_PARTY_LICENSES/additional/provenance.json`.

`CODE_OF_CONDUCT.md` is adapted from the [Contributor Covenant](https://www.contributor-covenant.org/version/3/0/), version 3.0, stewarded by the Organization for Ethical Source and licensed under the Creative Commons Attribution-ShareAlike 4.0 International License (CC BY-SA 4.0). The adapted text remains under CC BY-SA 4.0, and its Attribution section lists the changes.
