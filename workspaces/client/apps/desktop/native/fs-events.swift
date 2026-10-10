import CoreServices
import Foundation

// studio-fs-events ROOT...: file events below the roots for Studio's Mac folder sync
// (docs/vm-mac-sync.md). One JSON line per changed path; {"rescan": root} when macOS
// dropped events or the root changed, so the backend scans that folder in full.
// The helper exits when the backend closes its standard input.

setvbuf(stdout, nil, _IOLBF, 0)
let roots = Array(CommandLine.arguments.dropFirst())
guard !roots.isEmpty, roots.allSatisfy({ $0.hasPrefix("/") }) else {
    FileHandle.standardError.write("Use studio-fs-events ABSOLUTE_ROOT...\n".data(using: .utf8)!)
    exit(2)
}

let output = DispatchQueue(label: "studio.fs-events.output")
func emit(_ value: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: value),
          let line = String(data: data, encoding: .utf8) else { return }
    output.sync { print(line) }
}

let rescanFlags = UInt32(kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagUserDropped
    | kFSEventStreamEventFlagKernelDropped | kFSEventStreamEventFlagRootChanged)
let callback: FSEventStreamCallback = { _, _, count, paths, flags, _ in
    guard let list = unsafeBitCast(paths, to: NSArray.self) as? [String] else { return }
    for index in 0..<count where index < list.count {
        if flags[index] & rescanFlags != 0 {
            emit(["rescan": list[index]])
        } else {
            emit(["path": list[index]])
        }
    }
}

var context = FSEventStreamContext(version: 0, info: nil, retain: nil, release: nil, copyDescription: nil)
let creation = UInt32(kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer
    | kFSEventStreamCreateFlagWatchRoot | kFSEventStreamCreateFlagUseCFTypes)
guard let stream = FSEventStreamCreate(nil, callback, &context, roots as CFArray,
                                       FSEventStreamEventId(kFSEventStreamEventIdSinceNow), 0.3, creation) else {
    FileHandle.standardError.write("Cannot create the file event stream.\n".data(using: .utf8)!)
    exit(1)
}
FSEventStreamSetDispatchQueue(stream, DispatchQueue(label: "studio.fs-events"))
guard FSEventStreamStart(stream) else {
    FileHandle.standardError.write("Cannot start the file event stream.\n".data(using: .utf8)!)
    exit(1)
}
emit(["ready": roots])
DispatchQueue.global().async {
    while readLine() != nil {}
    exit(0)
}
dispatchMain()
