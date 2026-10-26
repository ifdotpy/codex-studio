# Layr Native

Layr Native runs the provider on the host. The lead uses the selected project folder.
Workers use separate Layr lines. The project folder stays at its original path.
The `.layr/link.json` file points to an external store. The store holds history, keys, and worker lines.

## Setup

Use a Layr binary that supports `workspace attach` and `workspace inspect`.
Set `STUDIO_NATIVE_LAYR_BINARY` to its absolute path in the Studio server environment.
Without this variable, Studio searches `PATH` for `layr`.

On macOS, Studio creates a case-sensitive APFS sparsebundle under its state directory:

```text
native-layr/storage/history.sparsebundle
native-layr/storage/volume/store
```

The image has a virtual capacity of 200 GB. It does not allocate 200 GB at creation.
Studio checks the image identity and enables ownership when it mounts the image.
An existing image with an unknown identity stops preparation.

On Linux, configure a Layr service before you select Layr Native.
Set these variables in the Studio server environment:

```sh
export STUDIO_NATIVE_LAYR_BINARY=/absolute/path/to/layr
export STUDIO_NATIVE_LAYR_ROOT=/absolute/path/to/store
export STUDIO_NATIVE_LAYR_SOCKET=/absolute/path/to/layr.sock
```

The root must match the service store. The socket must belong to that service.
Use the btrfs binary for a btrfs store. Use the OverlayFS binary for an ext4 or XFS store.
Studio does not create the Linux service or change its permissions.

## Chat behavior

Select **Layr Native** when you create the chat.
Studio imports the source, then attaches its main line to the original folder.
A failed attachment prevents the provider from starting.
The provider and command monitors receive the explicit project, line, store, and socket environment.

Each worker starts from a recorded Layr state in a separate line.
Task submission records the exact state. Task acceptance merges that state into the lead line.
Later worker changes do not change the submitted state.
The merge covers the submitted state, not separate worker overlays.

Studio saves completed turns with durable receipts.
After a lost response, Studio checks the operation history before another attempt.
An unknown result remains an error. Studio does not repeat an uncertain write.

Archive keeps the native line and its data. A pending turn save prevents archive.
Restore checks the line identity before another provider turn.
Review uses a separate line and the provider's read-only sandbox.
This sandbox is not a filesystem permission boundary against other host processes.

## Current limits

- The attached folder cannot move or be replaced. There is no detach or repair command yet.
- Older Layr binaries do not understand attached folders. Do not use them with an attached store.
- Source capture checks for concurrent changes. It is not an atomic filesystem snapshot.
- Native workers stay on the chat server. Remote native worker requests fail before creation.
- Native chats cannot teleport between servers. Their folder and store identity stays on the original server.
- Layr VM always uses btrfs. The Linux-host VM launcher is not implemented; that choice remains disabled on Linux.
- Existing image chats keep their saved mode.

## Verification

Disposable CLI tests cover APFS, btrfs, ext4, and XFS.
They cover source edits, worker isolation, merge, caches, transfer, and original Git metadata.
Linux service tests cover the owner and another user.
The runtime contract uses a fake provider to check preparation, review, worker creation, and task acceptance.
Browser tests cover the rendered choice for macOS and Linux server fixtures.
They check the selected mode and original folder in the chat creation request.
These checks do not prove a live provider session.
