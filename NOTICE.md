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

The bundled Monty interpreter is copyright (c) Pydantic Services Inc., licensed under the MIT License. See `THIRD_PARTY_LICENSES/Monty.txt`.

The file, shell, web, code, and Python tools are built on [Workcell](https://github.com/caudra/caudra/tree/main/workcell), copyright The Workcell MCP authors, licensed under the Apache License, Version 2.0. See `THIRD_PARTY_LICENSES/Workcell.txt`.

The Workcell shell output filter includes declarative filter rules copied without modification from [RTK](https://github.com/rtk-ai/rtk) at revision `aa408534859949ebac1dcc82ec4d25b575a539fa`, copyright 2024 rtk-ai and rtk-ai Labs, licensed under the Apache License, Version 2.0. See `THIRD_PARTY_LICENSES/RTK.txt`.

The patched Crossterm terminal library retains its MIT license in `THIRD_PARTY_LICENSES/Crossterm.txt` and `vendor/crossterm/LICENSE`.

Workcell includes adapted Ripwire and tree-sitter queries. Their notices and recorded provenance are in `THIRD_PARTY_LICENSES/Ripwire.txt` and `THIRD_PARTY_LICENSES/TreeSitterQueries.txt`. The original Ripwire import revision is not recorded. The documented inspection revision is not an import provenance claim.

The bundled two-face syntax collection includes separately licensed syntax definitions. See `THIRD_PARTY_LICENSES/TwoFace.txt` and `THIRD_PARTY_LICENSES/TwoFaceSyntaxes.txt` for the library license and syntax acknowledgements.
