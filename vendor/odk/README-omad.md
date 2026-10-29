# Vendored ODK

ODK as of commit `8e8f2d25c1e76e7dbcb2d811c6ccc7a8db795289`, minus `crates/oma`,
`crates/oma-consent` and `examples/` (an application bundle format this project does not
use yet), with the smallest patches needed to build outside Linux:

| File | Change | Why |
|---|---|---|
| `crates/odk-ui/src/input.rs` | `read_from_primary()` moved behind `paste_source()`: primary selection on Linux/FreeBSD, clipboard elsewhere | GPUI only defines `read_from_primary` on X11/Wayland; macOS and Windows have only the clipboard |
| `Cargo.toml` | members trimmed; the `xattr` patch removed | The patch belongs in the consuming workspace (cargo ignores patches in dependencies), and this copy is not built as a workspace of its own |

`vendor/xattr` carries the other half: ODK's Linux `ENOATTR` patch redefined that constant on
macOS, where libc already has it.

Both should go upstream. Until they do, `cargo update` on ODK means re-applying these three
edits; they are listed here so that is a five-minute job rather than an archaeology one.

## Licence

ODK is MIT (see `LICENSE` in this directory, copied from the upstream project's
`Cargo.toml` licence field). The patches listed here are released under the same MIT terms
as the rest of OmaLLDownloader.
