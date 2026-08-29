# Third-party notices

## PlayTools

This repository includes [PlayCover/PlayTools](https://github.com/PlayCover/PlayTools)
as a Git submodule pinned to commit
`d44d418b5221d06b934dbad80459705042c16b7b`.

PlayTools is licensed under the GNU Affero General Public License, version 3.
Its copyright and license text are preserved in
`third_party/PlayTools/LICENSE`.

Shiny Song Tools is licensed under the GNU General Public License, version 3.
Including PlayTools as a submodule does not replace or relax either project's
license obligations. Any future distributed work that incorporates or modifies
PlayTools code must preserve the upstream notices and provide the corresponding
source and modification information required by the AGPLv3.

## il2cpp-bridge-rs

The Rust runtime depends on [Batchhh/il2cpp-bridge-rs](https://github.com/Batchhh/il2cpp-bridge-rs),
pinned to version 0.1.4 (`=0.1.4` in the workspace manifest), as the IL2CPP
API table and metadata query layer.

il2cpp-bridge-rs is licensed under the MIT license. Its exact version,
upstream repository, and license are recorded here because the runtime pins
the dependency and its behavior (export set, cache initialization, internal
domain reads) is part of the validated evidence chain described in
`docs/`.
