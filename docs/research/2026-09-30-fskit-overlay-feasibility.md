# FSKit overlay feasibility for agent workspaces, 2026-09-30

Question: can Apple FSKit serve an overlay (union) file system for Codex Studio agent workspaces? The lower layer is the user's checkout, read-only. The upper layer is a writable directory for each agent. The options are A (FSKit overlay), B (APFS clone of the whole tree for each agent), and C (local user-space NFS server, like EdenFS).

This Mac: macOS 27.0, build 26A428 (`sw_vers`). Xcode SDK: MacOSX27.0. Command Line Tools also have MacOSX26.5.sdk. Nothing was installed or mounted for this research.

Header paths below are short forms:

- `H27` = `/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/System/Library/Frameworks/FSKit.framework/Headers`
- `H26` = `/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk/System/Library/Frameworks/FSKit.framework/Headers`

## Answer

1. Feasible on the API level from macOS 26: `FSPathURLResource` serves a directory, and a normal user can mount it on a directory they own. Apple ships a passthrough sample.
2. Limits: no root mounts, per-user approval in System Settings, one volume per module instance, and sandbox access to the second (upper) directory needs an entitlement exception or a path option.
3. Every cache miss goes over XPC to the module (an Apple engineer: about 121 µs per `getdirentries` "is about where things stand"). Kernel-offloaded I/O needs a block device. No clonefile, lock, fsync, or change-notification path to the module.
4. Apple DTS: the read/write path performance "is not great". Known bugs: negative lookups cached forever, `RENAME_SWAP` destroys the target. Chromium or cargo builds on it are a high risk.
5. Recommendation: use B now. Apple DTS itself recommends clones over a projection file system. Keep C as the fallback if chromium clone or delete time is too slow. Do A only as a measured prototype.

## Decision table

| Criterion            | A. FSKit overlay                                                                                                                                                                                                                                       | B. APFS clone (`cp -c`)                                                                                                                                  | C. Local NFS server                                                                                                                                                 |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Creation time        | About constant (one mount). Not measured.                                                                                                                                                                                                              | Grows with file count. 564 files: 0.06 s on this Mac. Chromium (1,090,996 files): measured separately.                                                   | About constant (one mount). EdenFS does this for large repos.                                                                                                       |
| Runtime speed        | Every cache miss goes over XPC to the module. About 121 µs per `getdirentries` in one forum test. Apple: read/write path "not great". macFUSE: FSKit I/O "not on par" with kext.                                                                       | Native APFS. No extra cost.                                                                                                                              | Every uncached call goes over loopback RPC. EdenFS: macOS NFS and FUSE speed were "pretty much a wash". Kernel NFS client caches attributes (5 to 60 s by default). |
| Write semantics      | Our code must do copy-up, whiteouts, rename, and hard links. Locks stay in the kernel (fine on one Mac). No clonefile, no fsync to the module, no rename flags. Changes made to the lower directory outside the mount are not announced to the kernel. | Full APFS semantics. Each agent gets an independent tree. Changes to the checkout after the clone do not show. Clone works only on the same APFS volume. | Our code must do copy-up and whiteouts. NFSv3 has no xattrs. Known stale-content and hang problems.                                                                 |
| Install and approval | Developer ID app with an `.appex` and a provisioning profile. Each user must turn on the extension in System Settings. An app update stops the extension and so the mounts. macOS 26+ for path resources, 27+ for the app mount API.                   | None.                                                                                                                                                    | A root helper to call `mount` (EdenFS uses one). No extension approval. Apple `nfs.kext` is built in.                                                               |
| Cross-platform fit   | macOS only. Linux has kernel overlayfs; Windows has ProjFS. Three different code paths.                                                                                                                                                                | macOS APFS only. Other systems need their own copy method.                                                                                               | The NFS server code can be shared by macOS and Linux (inference). The mount step is different on each OS. EdenFS uses ProjFS on Windows.                            |
| Effort               | High. New Swift extension, overlay logic, inode map, reclaim, cache rules. FSKit API still changes each year.                                                                                                                                          | Low. One syscall or `cp -c -R`. Cleanup of many files is the main cost.                                                                                  | Very high. An NFS server, overlay logic, and a root mount helper.                                                                                                   |

## Findings

### 1. FSKit availability by macOS version

- macOS 15.4 has the first API. `H27/FSKitDefines.h:36-38`: `// original API` `#define FSKIT_API_AVAILABILITY_V1 API_AVAILABLE(macos(15.4))`.
- macOS 26.0 adds a second API level. `H27/FSKitDefines.h:42-44`: `// macOS 26 API` `FSKIT_API_AVAILABILITY_V2 API_AVAILABLE(macos(26.0))`.
- macOS 26.4 adds `FSKIT_API_AVAILABILITY_V2_4` (`H27/FSKitDefines.h:46-48`). It adds only `requestedMountOptions` with `FSMountOptionsReadOnly` (`H27/FSVolume.h:66-71`, `:413`).
- macOS 27.0 adds a third level and deprecates much of the first API. `H27/FSKitDefines.h:50-58`: `FSKIT_API_AVAILABILITY_V3 API_AVAILABLE(macos(27.0))` and `API_DEPRECATED_WITH_REPLACEMENT(replacement, macos(15.4, 27.0))`.
- New headers in the 27.0 SDK, compared with 26.5: `FSContext.h`, `FSFreeSpace.h`, `FSVolumeDataCacheHandler.h`, `FSVolumeHandlerResult.h` (`diff` of `H26` and `H27`).
- macOS 27 replaces every `FSVolume...Operations` protocol with a `...Handler` protocol. Example: `H27/FSVolume.h:472`: `FSKIT_API_INTRODUCED_V1_DEPRECATED_V3_WITH_REPLACEMENT("FSVolumeHandler")`. Code written for 26 still builds but uses deprecated API.
- This Mac already runs FSKit. `pluginkit -mAv -p com.apple.fskit.fsmodule` lists Apple modules for exfat, msdos, ftp, and Xcode's `DeviceFS`. Daemons: `/usr/libexec/fskitd`, `/usr/libexec/fskit_agent`.
- macOS 15.4 release notes: "FSKit is now available, enabling delivery of user space file systems as Application Extensions." (https://developer.apple.com/documentation/macos-release-notes/macos-15_4-release-notes).
- Apple's "Building a passthrough file system" sample needs macOS 26.0 and Xcode 26.0 (https://developer.apple.com/documentation/fskit/building-a-passthrough-file-system).

### 2. Resource types and access to other directories

- `FSBlockDeviceResource`: macOS 15.4 (`H27/FSResource.h:150-151`). For disk partitions.
- `FSGenericURLResource`: macOS 26.0 (`H27/FSResource.h:391-392`). "FSKit leaves interpretation of the URL and its contents entirely up to your implementation" (`:380`). Schemes go in `FSSupportedSchemes` (`:382`).
- `FSPathURLResource`: macOS 26.0 (`H27/FSResource.h:409-410`). "A resource that represents a path in the system file space" (`:405`). It takes a `file:` URL and a `writable` flag (`:417-420`). "If the URL is a security-scoped URL, FSKit transports it intact from a client application to your extension" (`:407-408`). The 26.5 SDK has the same classes at the same level (`H26/FSResource.h:389-408`).
- `man 8 fsck_fskit` names three resource kinds. Path URL "covers a file system using a pre-existing volume as its data storage". The example is `file:///private/tmp/workdir`. It also says "the fsck_fskit command shares path access privileges with the FSModule".
- So yes, a file system can be backed by a directory, not a block device. Apple ships one: Xcode's `DeviceFS.appex` has `FSSupportsPathURLs = true` and `FSSupportsBlockResources = false` (`plutil -p .../DeviceFS.appex/Contents/Info.plist`).
- Extensions run in the App Sandbox. The Xcode template sets `ENABLE_APP_SANDBOX = YES` (`.../File System Extension.xctemplate/TemplateInfo.plist:26-27`). Apple's own ftp, msdos and DeviceFS modules all have `com.apple.security.app-sandbox = true` (`codesign -d --entitlements -`).
- A second directory (the upper layer) can come in as a path option. `H27/FSTaskOptions.h:26-30`: "Some command-line options refer to paths that indicate a location in which the module needs access to a file outside of its container. FSKit passes these paths as a URL tagged by the option name." The module lists such options in `pathOptions` inside `FSActivateOptionSyntax`.
- The sandbox is required. Quinn (Apple DTS, Apr 2025): "FSKit modules are packaged as app extensions and all app extensions must be sandboxed. They fail to load otherwise." (https://developer.apple.com/forums/thread/779672).
- Outside the Mac App Store the sandbox can be widened. Same post: "Code that ships outside of the App Store has a supported way to bypass most sandbox restrictions." Apple's DeviceFS does this with `com.apple.security.temporary-exception.files.home-relative-path.read-write` for `/Library/Developer/CoreSimulator/Devices/` (`codesign -d --entitlements -` on `DeviceFS.appex`).
- Path options are not reliable yet. In the same thread, `url(forOption:)` returned nil for a declared path option. Quinn: "we're struggling to think of a reason why this is failing ... this is likely to end up as a bug report." For the option type he says: "That's either `Path` or `Directory`". No fix is posted.
- A community developer built a unionfs-like FSKit module. They report: "I tried `com.apple.security.temporary-exception.files.absolute-path.read-write` with `/` and it worked." No Apple staff replied (https://developer.apple.com/forums/thread/808246).
- Info.plist keys seen in the Xcode template (`TemplateInfo.plist:151-171`): `FSPersonalities`, `FSRequiresSecurityScopedPathURLResources`, `FSShortName`, `FSSupportsBlockResources`, `FSSupportsGenericURLResources`, `FSSupportsPathURLs`, `FSSupportsServerURLs`.
- Apple's sample uses `FSPathURLResource` for any directory: `mount -t passthrough ~/Documents ~/passthrough-fs` (https://developer.apple.com/documentation/fskit/building-a-passthrough-file-system). Kevin Elliott (Apple DTS, Apr 2026) warns that the sample "uses the (slow) readdir/stat pattern, but appears to leak the directory descriptor and has a serious issue with its telldir() usage (r.175523886)". He says it should "NOT" be used "as a direct model" (https://developer.apple.com/forums/thread/824156).
- For an overlay, the design is: lower = `FSPathURLResource` (read-only), upper = a path option or an entitlement exception (read-write). The module then opens files under both with normal POSIX calls. That this works end to end in the sandbox: unverified until a prototype runs.

### 3. File system models and many mounts

- Only the unary model works. `H27/FSFileSystem.h:20-21`: "The current version of FSKit supports only `FSUnaryFileSystem`, not `FSFileSystem`." `FSFileSystem` is `FSKIT_API_UNAVAILABLE_V1`. The Swift `FileSystemExtension` protocol is `@available(macOS, unavailable)` (`.../FSKit.swiftmodule/arm64e-apple-macos.swiftinterface:53-61`).
- `H27/FSUnaryFileSystem.h:16`: "`FSUnaryFileSystem` is a simplified file system, which works with one `FSResource` and presents it as one `FSVolume`."
- `loadResource` gets only two options: "`-f` for "force" and `--rdonly` for read-only" (`H27/FSUnaryFileSystem.h:51`). Other module options come through `FSActivateOptionSyntax`.
- So 10 to 30 agents need 10 to 30 resources, and so 10 to 30 volumes. Whether they run in one extension process or many, and whether there is a limit: unverified. macFUSE recommends one process per volume for its own library (macfuse issue #1059, maintainer).

### 4. Mounting

- Command line: `mount -t <shortname> <resource> <mountpoint>`. For a path module the resource is a `file://` URL or a path (`man 8 fsck_fskit`; Apple sample). `-F` forces FSKit. `man 8 mount`: "Only needed by developers migrating from a traditional Filesystems bundle ( /Library/Filesystems/*.fs ) to FSKit."
- App API, macOS 27 only: `FSClient.mountSingleVolumeForResource:bundleID:options:completionHandler:` (`H27/FSClient.h:36-53`). It needs the `com.apple.developer.fskit.mount` entitlement. "The system mounts the volume within the `/Volumes/` directory" (`:40`). "The caller can only mount modules that are visible to them" (`:42`). The 26.5 SDK does not have this method (`diff H26/FSClient.h H27/FSClient.h`).
- `FSClient.openFileSystemExtensionsSettings` (macOS 27) opens the System Settings pane where "they can view, enable, and disable file system extensions" (`H27/FSClient.h:55-61`).
- Mount point with `mount(8)`: a directory the user owns works. Apple's sample mounts at `~/passthrough-fs` with no `sudo` (sample page above). A forum user shows `file:///Users/alexf/Downloads/ on /Users/alexf/mnt (passthrough, local, nodev, nosuid, noowners, noatime, fskit, mounted by alexf)` (https://developer.apple.com/forums/thread/820931). On this Mac, Apple's DeviceFS is mounted at `/Users/igor/Library/Developer/CoreDevice/DeviceFS` with `fskit, mounted by igor` (`mount`).
- Mount point with the macOS 27 app API: only `/Volumes` (`H27/FSClient.h:40`). `strings /usr/libexec/fskitd` shows a private check, `Client missing 'com.apple.private.fskit.mountAt' entitlement for path`. So an app that wants another path must run `mount(8)` (inferred from strings).
- Conflict: the macFUSE wiki says "Using mount points outside of /Volumes is not supported by FSKit" (https://github.com/macfuse/macfuse/wiki/FUSE-Backends; issue #1116, 2025-09). Apple's sample and the forum output above show user-owned paths work with `mount(8)`. The macFUSE rule is likely its own daemon design or an older macOS limit (unverified).
- `fskitd` has the string `caller euid %u not authorized to mount on %s (owner %u)`. This suggests the caller must own the mount point. Unverified.
- Who can mount: a normal user. Root mounts of third-party modules fail. macFUSE issue #1169, maintainer: "Mounting volumes (using `FSKit`) as `root` fails. This is a `FSKit` limitation". An Apple Systems Engineer (Jun 2026): "AppEx state is per-user. In fact, root's LaunchServices database is typically very empty ... our current consent/approval model only asks a user for approval for themselves." (https://developer.apple.com/forums/thread/831396).
- Before macOS 27 there is no mount API for apps. Kevin Elliott (Sep 2025): "In practice, '/sbin/mount' is the system's primary mounting interface." Also: "the way diskarbitrationd ultimately mounts volumes is by using posix_spawn to run '/sbin/mount'." (https://developer.apple.com/forums/thread/799283).
- Ownership: FSKit mounts show `noowners` (both outputs above). `man 8 mount`: noowners means "Ignore the ownership field for the entire volume". So all files likely show the mounting user as owner. Unverified for a module that asks otherwise.
- Finder and Spotlight: a user reports `nobrowse` is ignored. Kevin Elliott (Mar 2026): "Yes, it should be supported." For Spotlight: "I believe adding a file named `.metadata_never_index` at the root of the volume will prevent this." (thread 820931).

### 5. Operations support

From `H27/FSVolume.h` (macOS 27 `FSVolumeHandler` and related protocols):

| Operation                                         | Status                                                                                                                                     | Evidence                                                                                                                                                                                                               |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Create, remove, lookup, readdir, getattr, setattr | Yes                                                                                                                                        | `FSVolume.h:817-1028`                                                                                                                                                                                                  |
| Read, write                                       | Yes, through the module. Kernel-offloaded I/O is for block devices only.                                                                   | `FSVolume.h:1371-1408`, `FSVolumeExtent.h:8-14`, `:281-283`                                                                                                                                                            |
| Rename                                            | Yes, no flags parameter. `RENAME_SWAP` today destroys the target (bug).                                                                    | `FSVolume.h:927-935`. FB24419773; Kevin Elliott: "This is entirely a bug" (https://developer.apple.com/forums/thread/842736).                                                                                          |
| Symlinks                                          | Yes                                                                                                                                        | `FSVolume.h:861-867`, `:1036-1039`                                                                                                                                                                                     |
| Hard links                                        | Yes                                                                                                                                        | `FSVolume.h:883-888`; capability flag `supportsHardLinks` `:143-144`                                                                                                                                                   |
| Extended attributes                               | Yes, optional protocol                                                                                                                     | `FSVolumeXattrHandler`, `FSVolume.h:1131-1197`                                                                                                                                                                         |
| Open, close, mmap                                 | Yes. "When all memory mappings to the item release, the kernel layer issues a final close."                                                | `FSVolume.h:1264`                                                                                                                                                                                                      |
| Open-unlink                                       | FSKit can emulate it, macOS 26+                                                                                                            | `enableOpenUnlinkEmulation`, `FSVolume.h:397-407`                                                                                                                                                                      |
| Preallocate, SEEK_HOLE and SEEK_DATA              | Yes (seek is macOS 27)                                                                                                                     | `FSVolume.h:1637`, `:1769-1796`                                                                                                                                                                                        |
| clonefile                                         | No operation in any header                                                                                                                 | `grep -w clone` on `H27/*.h`: no match                                                                                                                                                                                 |
| Byte-range locks (`fcntl`, `flock`)               | No module operation. Locks "stay kernel-local and never reach the module". On one Mac this should be enough for cargo and git (inference). | `grep -w lock` on `H27/*.h`: no match. FB24419974, as described by the poster in thread 842736.                                                                                                                        |
| ioctl, `fsctl`, `F_FULLFSYNC`                     | No operation in any header. `synchronize` is never called on a URL-backed volume (FB24419870).                                             | `grep -w ioctl` on `H27/*.h`: no match. Kevin Elliott: these calls "are actually built on the generic VFS IOCTL system" and FSKit "will need to address this issue" (thread 842736).                                   |
| ACLs                                              | None                                                                                                                                       | FB24419979 (thread 842736)                                                                                                                                                                                             |
| Negative lookups                                  | Cached for the life of the vnode. A name created later from outside stays ENOENT.                                                          | FB24419825; Kevin Elliott: "Those are bugs as well." (thread 842736)                                                                                                                                                   |
| External change notification                      | None. Changes made directly in the lower or upper directory are not announced to the kernel.                                               | Kevin Elliott (Jun 2026): the cache API is "not informing the system about changes generated by external activity (in VFS terms, "vnode_notify")"; bug r.177724575 (https://developer.apple.com/forums/thread/832647). |
| Case sensitivity                                  | Module chooses: sensitive, insensitive, or insensitive case-preserving                                                                     | `FSVolumeCaseFormat`, `FSVolume.h:122-129`                                                                                                                                                                             |
| Caller identity                                   | macOS 27 only. `FSContext` has real and effective uid and gid.                                                                             | `H27/FSContext.h:13-27`                                                                                                                                                                                                |
| Permission checks                                 | Optional `FSVolumeAccessCheckHandler`                                                                                                      | `FSVolume.h:1485-1508`                                                                                                                                                                                                 |

- Effect of no clonefile: `cp -c` "will fallback to using copyfile(2)" when "the target filesystem does not support cloning" (`man 1 cp`). Inside an overlay mount, APFS clones become full data copies.
- Effect on builds: git takes its index lock with a lock file and `rename`, not a byte-range lock. Cargo locks its build directory with `flock`. Both statements are general knowledge, not checked for this report (unverified). Kernel-local locks work when all users of the lock are on the same Mac, so both should work. This needs a test.
- Effect on an overlay: if the user runs `git pull` in the lower checkout while agents run, the kernel does not learn of it. Cached names and attributes stay old. Names that were missing stay missing (negative lookup bug). An overlay must treat the lower layer as frozen, or remount after it changes.
- Apple's view of this design. Kevin Elliott (Aug 2026): "I know of one company with a very large source code base that uses it as the front end to their source code archive system ... most of the issues you've raised don't matter because the access is read-only and relatively static." An agent workspace is read-write, so these issues do matter (thread 842736).

### 6. Performance

- Kernel-offloaded I/O (KOIO) is for block devices only. `H27/FSVolumeExtent.h:8-14`: "For block device resource file systems, FSKit offers a facility called Kernel-Offloaded I/O (KOIO) ... the module supplies file extent mappings to the kernel and the kernel then performs data transfers directly". The extent packer takes an `FSBlockDeviceResource` (`:104`). A directory overlay has no block device, so all its data goes through `FSVolumeReadWriteHandler`.
- Kevin Elliott (Apple DTS, Sep 2025): "FSVolumeKernelOffloadedIOOperations works by passing dev node offsets into the kernel, so you can't use it without a dev node." (https://developer.apple.com/forums/thread/799283).
- Without KOIO, data moves between module and kernel. `H27/FSVolume.h:1361-1366`: "read and write operations that deliver data to and from the extension". Kevin Elliott, same thread: "We haven't done much to optimize the FSVolumeReadWriteOperations path, so its current performance is not great." The poster measured macFUSE at "about 40% of CPU" and FSKit at "between 100 and 150%".
- Metadata cost. A forum user measured about 121 µs per `getdirentries` on macOS 15.5. Kevin Elliott (Jul 2025): "It's reasonable in the sense that, yes, that's about where things stand at the moment", and the cost is "overhead of repeated XPC traffic". On caching: "some caching is going on but probably not enough to be truly useful." (https://developer.apple.com/forums/thread/793013).
- Directory listing. Kevin Elliott (Apr 2026): after `enumerateDirectory` the system will often "Generate lookupItem() calls for every entry. Needless to say, that's not ideal." He calls it "a performance bottleneck that we will need to address at some point." (https://developer.apple.com/forums/thread/824156).
- Caching in a filter file system. Same post: "in a filter file system the problem you have is that caching ANYTHING becomes dangerous". An overlay is a filter file system.
- Scale estimate (inference, not measured): at 121 µs per uncached call, 1,090,996 uncached `stat` calls take about 132 s of round trips. A warm kernel cache avoids most of this, but each agent mount has its own cold cache.
- Attribute caching: `H27/FSVolumeHandlerResult.h:34`: "FSKit caches all populated attributes and may use them in subsequent operations, even if not explicitly requested." In macOS 27, create, rename, remove and other results also return parent directory attributes (`FSVolumeHandlerResult.h:89-193`). This removes extra getattr round trips.
- There is no header API to set an attribute or name cache timeout. macFUSE 5.4.0 added its own "item attribute caching based on the validity timeouts returned by the file system server" (macFUSE 5.4.0 release notes).
- Data caching, macOS 27: `FSVolumeDataCacheHandler` lets the module grant read cache, write-through, or write-back per open file (`H27/FSVolumeDataCacheHandler.h:61-116`). "The protocol supports deferred closing, where the kernel maintains cache state even after a file is closed" (`:79-82`). Without this protocol "the kernel may still cache it" but the module "has no control over caching behavior" (`:112-113`).
- An Apple Systems Engineer (Jun 2026) on this protocol: "The key consideration here isn't block vs other resources, it is if any other system is simultaneously modifying the file system." (https://developer.apple.com/forums/thread/831652). An overlay whose lower checkout can change is such a case.
- Reclaim cost: `H27/FSItem.h:117`: FSKit and the kernel each count lookups for an item. The module must call `tryReclaim` on the right sync context. This is like FUSE `forget`.
- Published numbers: none from Apple in the headers. macFUSE wiki "FUSE Backends": "I/O performance of FSKit volumes is not on par with volumes using the kernel extension backend." macFUSE 5.3.0 notes: "reading files is up to 15 times faster than in previous releases". macFUSE discussion #1183 (2026-09-09): "it can still be considerably slower than the kernel backend, depending on the workload".
- Fit for large builds: poor today, by Apple's own statements above. Chromium has about 1.09 million files. A build does many stat and open calls. No source shows FSKit numbers for such a load.
- Apple's advice for build sandboxes. Kevin Elliott (Jun 2026), answering a Bazel projection-file-system question: "Copy/clone the entire hierarchy into a "private" location, so you're now working on an isolated copy." He adds that disk images plus EndpointSecurity could be "MANY orders of magnitude faster than a traditional "projection" based approach." (https://developer.apple.com/forums/thread/828533). The poster answered that Bazel rewrites files all the time and that clonefile does not work across volumes.

### 7. Distribution

- Extension point: `com.apple.fskit.fsmodule`. The module is an ExtensionKit `.appex` inside an app (Xcode template `File System Extension.xctemplate`, `TemplateInfo.plist:133`).
- Entitlement `com.apple.developer.fskit.fsmodule`: "Marks an ExtensionKit extension as being a FSKit filesystem." Xcode's portal capability cache lists it with `"distributionApprovalRequired" : false`, distribution types Ad hoc, Developer ID, Development, and App Store Connect, and team types Apple Developer Program and Enterprise (`/Applications/Xcode.app/Contents/SharedFrameworks/DVTPortal.framework/Versions/A/Resources/DVTPortalCachedPortalCapabilities.json:7282`, id `FSKIT_MODULE` at `:7306`).
- Entitlement `com.apple.developer.fskit.mount`: "Enables an app to mount its FSKit module". Same flags: no approval needed, Developer ID allowed (same file `:7341`, id `FSKIT_MOUNTER` at `:7365`).
- The entitlement is restricted. Quinn (Apple DTS, Mar 2026): "That's a _restricted_ entitlement, which means it must be authorised by a provisioning profile ... To create that profile, you'll need to be a member of a paid team." (https://developer.apple.com/forums/thread/817501). There is no extra approval form (`distributionApprovalRequired: false` above).
- So a non-App-Store Electron app can ship an FSKit module. It needs a paid team, a Developer ID provisioning profile, notarization (general Developer ID rule; no FSKit-specific statement found), and a native extension target. Electron has no FSKit support, so the extension and a small native mount helper are separate code.
- App updates stop mounts. Kevin Elliott (Dec 2025): "FSKit is built on top of our standard app extensions infrastructure, which is then terminating your extensions as part of its normal process for handling these cases." His advice: keep "some kind of "app experience" running while your volume is mounted" (https://developer.apple.com/forums/thread/809747). Codex Studio live updates would need to unmount agent workspaces first.
- The user must turn on the extension. `FSModuleIdentity.enabled` exists (`H27/FSModuleIdentity.h:23-25`). `openFileSystemExtensionsSettings` opens the settings pane (`H27/FSClient.h:55-61`). macFUSE's `MFMount` result `MFMountResultFileSystemExtensionRequiresApproval`: "The user needs to approve or enable the file system extension in System Settings" (https://github.com/macfuse/macfuse/wiki/Getting-Started-(Developer)-‐-MFMount.framework).
- No kernel extension, no Recovery Mode change. macFUSE Getting Started wiki: "It does not require a kernel extension or any security configuration changes in Recovery Mode."

### 8. macFUSE FSKit backend

- Added in macFUSE 5.0.0 (2025-05-05): "Add experimental support for `FSKit` on macOS 15.4 and newer". Selected with "`-o backend=fskit`" (https://github.com/macfuse/macfuse/releases/tag/macfuse-5.0.0).
- The kernel backend is still the default. Discussion #1183: "For the foreseeable future, the kernel extension will remain the default macFUSE backend."
- Latest release: 5.4.0, 2026-09-07 (https://github.com/macfuse/macfuse/releases/tag/macfuse-5.4.0).
- Limits stated by macFUSE:
  - Mount points only in `/Volumes` (wiki "FUSE Backends"; issue #1116).
  - No root mounts (issue #1169).
  - "Files are always opened in read/write mode"; "The FUSE notification API is not supported, yet" (wiki "FUSE Backends").
  - Notifications are ignored "because FSKit provides no API to which they can be forwarded" (issue #1167). So the file system cannot tell the kernel to drop stale entries.
  - If the server process dies, the volume hangs. The workaround is to kill `fskitd` (issue #1201).
  - No swap renames (5.4.0 notes).
  - Open issue #1206: mounts fail on macOS 27.0.
- License: "Redistributions in binary form, bundled with commercial software, are not allowed without specific prior written permission" (https://raw.githubusercontent.com/macfuse/macfuse/release/macfuse/LICENSE.txt). The FSKit modules install system-wide in `/Library/Filesystems/macfuse.fs`. Installing the mount daemon needs an administrator password (`Mounter.swift` in https://github.com/macfuse/mount).
- unionfs-fuse: the README says it was "successfully compiled and run on MacOS (with the help of macfuse - formerly osxfuse)" (https://github.com/rpodgorny/unionfs-fuse). Its CI only builds it. Copy-up does full file copies, not clones (issue #119, open). No report of it on the FSKit backend: unverified.
- fuse-overlayfs does not support macOS: "MAC OS support is out of scope for fuse-overlayfs" (https://github.com/containers/fuse-overlayfs issue #140).

### 9. Known limits, forum posts, WWDC

- WWDC: no session is about FSKit. A search of WWDC24, WWDC25 and WWDC26 session transcripts for "FSKit" found one mention only, in the WWDC24 Platforms State of the Union: "new APIs including user space file system support" (https://developer.apple.com/videos/play/wwdc2024/102/). A helper agent did this search; it is not re-checked (unverified).
- The main Apple sources are DTS posts on the Developer Forums (tag FSKit, https://developer.apple.com/forums/tags/fskit). Posts used in this report, all by Apple staff:

| Thread | Author, date               | Point                                                                                                                                  |
| ------ | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| 779672 | Quinn, Apr 2025            | Extensions must be sandboxed. Direct distribution can widen the sandbox. `url(forOption:)` returned nil, cause unknown.                |
| 793013 | Kevin Elliott, Jul 2025    | About 121 µs per `getdirentries` "is about where things stand". Cost is XPC traffic.                                                   |
| 799283 | Kevin Elliott, Sep 2025    | Read/write path "not great". KOIO needs a dev node. `/sbin/mount` is the mount interface.                                              |
| 809747 | Kevin Elliott, Dec 2025    | App updates terminate the extension and so the mounts.                                                                                 |
| 817501 | Quinn, Mar 2026            | `fsmodule` is a restricted entitlement. Needs a provisioning profile and a paid team.                                                  |
| 820931 | Kevin Elliott, Mar 2026    | `nobrowse` "should be supported". Use `.metadata_never_index` against Spotlight.                                                       |
| 824156 | Kevin Elliott, Apr 2026    | `lookupItem` for every listed entry is a bottleneck. Caching in a filter file system is dangerous. Do not copy the passthrough sample. |
| 828533 | Kevin Elliott, Jun 2026    | For build sandboxes, clone the whole hierarchy. Could be "MANY orders of magnitude faster" than projection.                            |
| 831396 | Systems Engineer, Jun 2026 | Extension state and approval are per user. Root mounting is not a supported pattern.                                                   |
| 831652 | Systems Engineer, Jun 2026 | Data cache protocol matters when another system changes the files at the same time.                                                    |
| 832647 | Kevin Elliott, Jun 2026    | No change notification (`vnode_notify`); bug r.177724575.                                                                              |
| 842736 | Kevin Elliott, Aug 2026    | `RENAME_SWAP`, negative lookup cache, and wedged activate are bugs. FSKit fits "read-only and relatively static" use.                  |

- Bug reports filed by developers and seen in these threads: FB24419773 (`RENAME_SWAP` destroys target), FB24419825 (negative lookup cached forever), FB24419858 (data-cache grant after invalidate), FB24419870 (`synchronize` never called on URL volumes), FB24419911 (`restrictsOwnershipChanges` not enforced), FB24419932 (failed activate wedges the URL), FB24419974 (no byte-range locks), FB24419979 (no ACLs). All from https://developer.apple.com/forums/thread/842736.
- macFUSE reports more FSKit problems: a zero-length read is reported as a full read, "This seems to be a FSKit issue" (macFUSE issue #1196); if the server process dies the volume hangs until `fskitd` is killed (issue #1201); mounts fail on macOS 27.0 (issue #1206, open).

### 10. Option C: EdenFS and local NFS

- EdenFS on macOS is an NFSv3 server on localhost. `eden/fs/docs/macOS.md:3-6`: "On macOS EdenFS uses NFS ... EdenFS is an NFSv3 server." Permalink base: https://github.com/facebook/sapling/blob/fcf7699/.
- It listens on `127.0.0.1` with a kernel-chosen port (`eden/fs/utils/NfsSocket.cpp:23`). A root helper loads `nfs.kext` and calls `mount("nfs", ...)` (`eden/fs/privhelper/PrivHelperServer.cpp:710`, `:964`).
- Mount settings: NFSv3 over TCP, local locks (`NFS_LOCK_MODE_LOCAL`, `PrivHelperServer.cpp:820`), hard mounts on macOS, `readdirplus` off, 16 KiB read and write size (`eden/fs/config/EdenConfig.h:1239`, `:1252`, `:1350`, `:1391-1393`).
- Why NFS and not FUSE. `macOS.md:24-38`: "Apple deprecated third party kernel extensions and has made them harder and harder to use." "As of Ventura, third party kernel extensions require turning off SIP ... So we force migrated everyone to NFS on Ventura." A 2026-08 commit (08a046124e) removed macOS FUSE: "macOS FUSE mount requests now fail with ENOTSUP."
- Performance statement. `macOS.md:180-184`: "generally performance on macOS is pretty close between the two ... When we rolled out NFS perf impacts across the board were pretty much a wash." No numbers.
- Known NFS problems from `macOS.md`:
  - No xattrs in NFSv3 (`:105-108`).
  - Stale content after checkout (`:121-137`).
  - readdir gives no entry types, so there are extra lookups; Buck2 hit this (`:139-151`).
  - No forget calls, so inode count grows (`:153-169`).
  - A dead server makes I/O hang (`:171-176`).
  - No close-to-open consistency on the macOS client (`:196-202`).
- FSKit status at Meta. `macOS.md:97-101`: "FSKit is fairly new (introduced in 2024). We have not thoroughly investigated FSKit, but it is worth investigating". An EdenFS engineer asked Apple about FSKit in October 2024 (https://developer.apple.com/forums/thread/766793): "NFS does not provide a `forget` API similar to FUSE ... performance issues are inevitable after some time", and "The above issues make us extremely motivated to use FSKit". An Apple Systems Engineer replied (Feb 2025): "FSKit's `reclaimItem` is very similar to FUSE's `forget` operation. FSKit does not support process attribution."
- EdenFS has an overlay. `eden/fs/docs/Glossary.md:137-149`: "The overlay is where EdenFS stores information about materialized files and directories". It works "where local modifications are overlaid on top of the underlying source control state". This is the same shape as option A and option C.
- Buildbarn picked NFSv4 on localhost for macOS. ADR 0009 (https://github.com/buildbarn/bb-adrs/blob/master/0009-nfsv4.md:31-37): macFUSE "does tend to cause system lockups under high load", so they chose "an integrated NFSv4 server that listens on `localhost`". NFSv3 is "far more 'chatty' than NFSv4" (`:45-48`).
- FUSE-T is "a kext-less implementation of FUSE for macOS that uses NFS v4 local server" (https://github.com/macos-fuse-t/fuse-t). It also has an FSKit backend for macOS 26+.
- Kernel NFS client caches attributes. `man 8 mount_nfs`: "The default minimum is 5 seconds and the default maximum is 60 seconds." This helps speed but makes upper-layer changes appear late to other readers.
- Whether a normal user can mount a localhost NFS server without root: unverified. EdenFS uses a root helper. `man 8 mount_nfs` says only that "root permission is required to mount using resvport".

### Option B notes

- `man 2 clonefile`: "If src names a directory, the directory hierarchy is cloned as if each item was cloned individually." Cost grows with file count.
- The same page: "Cloning directories with these functions is strongly discouraged. Use copyfile(3) to clone directories instead." `man 3 copyfile`: with `COPYFILE_RECURSIVE`, `COPYFILE_CLONE` "invokes copyfile() with COPYFILE_CLONE on every entry". `cp -c -R` does the same.
- Data blocks are shared until a write. "Subsequent writes to either the original or cloned file are private to the file being modified (copy-on-write)" (`man 2 clonefile`).
- Delete cost is also per file. Removing 30 chromium clones means removing about 33 million files. Not measured.
- Clones need the same volume. `man 2 clonefile`: "[EXDEV] src and dst are not on the same filesystem." Agent workspaces must live on the APFS volume of the checkout. If not, `cp -c` falls back to a full copy (`man 1 cp`).
- Apple DTS recommends this pattern for build sandboxes (thread 828533, section 6).

### Cross-platform notes

- Linux has a kernel overlay file system. "An overlay filesystem combines two filesystems - an 'upper' filesystem and a 'lower' filesystem." On a write, "the file is first copied from the lower filesystem to the upper filesystem (copy_up)" (https://docs.kernel.org/filesystems/overlayfs.html).
- Windows: "On Windows, EdenFS uses Microsoft's Projected File System" (`eden/fs/docs/Overview.md:41-42` in Sapling).
- So option A is macOS-only code. Linux gets overlay semantics from the kernel. Windows needs ProjFS or a copy.

## Open questions

1. Chromium clone time and delete time with `cp -c -R` or `copyfile(COPYFILE_CLONE | COPYFILE_RECURSIVE)`. If both take a few seconds, B wins and A is not needed.
2. Can a Developer ID FSKit module read the lower path and write the upper path from its sandbox? Does a `pathOptions` entry work now, or is a temporary-exception entitlement needed (thread 779672)?
3. Can a normal user mount it with `mount -t` on a directory in `~/Library/Application Support/` with no admin prompt? Does `nobrowse` work? Is `noowners` forced?
4. Do `flock`, `fcntl(F_SETLK)` and a writable `mmap` work on the volume?
5. Cost per `stat` and per `open` on a cache miss on macOS 27. Speed of `git status` and a cargo build on the mount versus native APFS.
6. Can one extension process serve 30 volumes? What is the memory per volume?
7. What happens to open files and builds if the module crashes? macFUSE reports hung volumes (issue #1201).
8. What do agents see after `git pull` in the lower checkout? The negative lookup bug (FB24419825) and the missing change notification (r.177724575) suggest old data.

## Minimal prototype

Do these steps on a test Mac with macOS 27, not in the user's work session.

1. Measure B first. Run `time cp -c -R <chromium> <dst>` and `time rm -rf <dst>`. Run each three times. Record the numbers. If both are short enough for agent start and cleanup, stop here and use B.
2. If B is too slow, build a passthrough FSKit module from the Xcode "File System Extension" template in Swift:
   - `FSSupportsPathURLs = true`. Lower = `FSPathURLResource` (read-only). Upper = a `Directory` path option `-u <path>`, or a temporary-exception entitlement if the option returns nil.
   - Implement `FSVolumeHandler`, `FSVolumeReadWriteHandler`, `FSVolumeXattrHandler`, `FSVolumeOpenCloseHandler`.
   - Lookup: upper first, then lower. Write on a lower file: copy-up to upper with `clonefile` (same APFS volume). Delete of a lower name: a whiteout marker in upper.
   - Return full attributes in every result so FSKit can cache them. Do not copy the Apple sample's readdir code (thread 824156).
3. Sign it with a Developer ID profile. Turn it on in System Settings. Mount with `mount -t <shortname> file:///path/lower ~/Library/Application\ Support/<app>/mnt/agent1`. Put `.metadata_never_index` at the root.
4. Run these checks on the mount and on native APFS, and compare:
   - `find . -type f | wc -l` and `git status` on chromium, cold and warm.
   - A `stat` loop over 100,000 files, cold and warm.
   - `cargo build` of a medium crate (checks `flock`).
   - `cp -c` of a big file (checks the clone fallback).
   - A writable `mmap` test, and `rename` over an open file.
   - A change in the lower checkout while mounted, then `stat` and `open` of the changed and new names.
   - 30 mounts at once. Record memory and mount time.
   - Kill the extension process during a build. Record what the kernel does.
5. Decision rule: choose A only if warm `git status` and a cargo build stay within 1.5 times native speed, and every check in step 4 passes. Otherwise use B. Use C only if B creation or cleanup is too slow for chromium and A fails.
