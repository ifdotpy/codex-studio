import Foundation
import Virtualization
import Darwin

// This process owns the VM independently of the Studio backend.
struct HostError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}
func fail(_ message: String) throws -> Never { throw HostError(message: message) }
func json(_ value: [String: Any]) -> Data {
    (try? JSONSerialization.data(withJSONObject: value)) ?? Data("{}".utf8)
}
func writeAll(_ fd: Int32, _ data: Data) -> Bool {
    data.withUnsafeBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            if count < 0 && errno == EINTR { continue }
            if count <= 0 { return false }
            offset += count
        }
        return true
    }
}
func reply(_ fd: Int32, _ value: [String: Any]) {
    _ = writeAll(fd, json(value) + Data([10]))
}
func address(_ path: String) throws -> sockaddr_un {
    var result = sockaddr_un()
    result.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8) + [0]
    guard bytes.count <= MemoryLayout.size(ofValue: result.sun_path) else {
        try fail("VM socket path exceeds the macOS limit. Use a shorter state directory.")
    }
    withUnsafeMutableBytes(of: &result.sun_path) { target in target.copyBytes(from: bytes) }
    return result
}

final class HostExecListener: NSObject, VZVirtioSocketListenerDelegate {
    weak var host: Host?
    init(host: Host) { self.host = host }
    func listener(_ listener: VZVirtioSocketListener, shouldAcceptNewConnection connection: VZVirtioSocketConnection) -> Bool {
        guard let host = host, host.channels.wait(timeout: .now()) == .success else { return false }
        DispatchQueue.global().async { host.connectHostExec(connection) }
        return true
    }
}

final class Host: NSObject, VZVirtualMachineDelegate {
    let directory: URL
    var vm: VZVirtualMachine?
    var phase = "starting"
    var failure: String?
    var lease: Int32 = -1
    var consolePipe: Pipe?
    var config: [String: Any] = [:]
    let channels = DispatchSemaphore(value: 64)
    let consoleQueue = DispatchQueue(label: "studio.vm.console")
    var hostExecListener: VZVirtioSocketListener?
    var hostExecDelegate: HostExecListener?
    // The read-only project share: 127.0.0.1 on the Mac to vsock 4052 in the guest.
    // Loopback needs no macOS Local Network permission and never touches the VM network.
    var sharePort: Int?
    var shareError: String?
    let shareChannels = DispatchSemaphore(value: 32)

    init(directory: URL) { self.directory = directory }
    func boot() throws {
        guard VZVirtualMachine.isSupported else { try fail("Virtualization.framework is unavailable on this computer.") }
        lease = open(directory.appendingPathComponent("host.lock").path, O_CREAT | O_RDWR, 0o600)
        guard lease >= 0, flock(lease, LOCK_EX | LOCK_NB) == 0 else {
            try fail("Another VM helper owns this state directory.")
        }
        // A new boot must not inherit a provision error from the previous boot.
        try Data().write(to: directory.appendingPathComponent("console.log"), options: .atomic)
        config = try JSONSerialization.jsonObject(with: Data(contentsOf: directory.appendingPathComponent("config.json"))) as? [String: Any] ?? [:]
        guard let cpus = config["cpus"] as? Int, let memory = config["memoryBytes"] as? UInt64,
              cpus >= VZVirtualMachineConfiguration.minimumAllowedCPUCount,
              cpus <= VZVirtualMachineConfiguration.maximumAllowedCPUCount,
              memory >= VZVirtualMachineConfiguration.minimumAllowedMemorySize,
              memory <= VZVirtualMachineConfiguration.maximumAllowedMemorySize else {
            try fail("Invalid VM CPU or memory limits.")
        }
        let configuration = VZVirtualMachineConfiguration()
        configuration.cpuCount = cpus
        configuration.memorySize = memory
        let bootloader = VZLinuxBootLoader(kernelURL: directory.appendingPathComponent("kernel"))
        bootloader.initialRamdiskURL = directory.appendingPathComponent("initrd")
        // NoCloud checks the kernel instance ID before it reads a changed seed disk.
        // Keep the old identity until the host publishes a recovery seed.
        let seedInfo = directory.appendingPathComponent("provision-seed.json")
        var instanceId = "studio-linux-v1"
        if FileManager.default.fileExists(atPath: seedInfo.path) {
            let metadata = try JSONSerialization.jsonObject(with: Data(contentsOf: seedInfo)) as? [String: Any]
            guard let value = metadata?["instanceId"] as? String,
                  value.range(of: "^studio-linux-[a-f0-9]{64}$", options: .regularExpression) != nil else {
                try fail("The VM provision seed identity is invalid.")
            }
            instanceId = value
        }
        bootloader.commandLine = "root=/dev/vda rw console=hvc0 ds=nocloud;i=" + instanceId
        configuration.bootLoader = bootloader
        configuration.platform = VZGenericPlatformConfiguration()
        configuration.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        configuration.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]
        let network = VZVirtioNetworkDeviceConfiguration()
        // Cloud-init matches its network configuration to the first boot's MAC.
        let macURL = directory.appendingPathComponent("network-mac")
        if FileManager.default.fileExists(atPath: macURL.path) {
            let saved = try String(contentsOf: macURL, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
            guard let mac = VZMACAddress(string: saved), mac.isUnicastAddress, mac.isLocallyAdministeredAddress else {
                try fail("The saved VM network MAC address is invalid.")
            }
            network.macAddress = mac
        } else {
            try network.macAddress.string.write(to: macURL, atomically: true, encoding: .utf8)
            chmod(macURL.path, 0o600)
        }
        network.attachment = VZNATNetworkDeviceAttachment()
        configuration.networkDevices = [network]
        configuration.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        configuration.storageDevices = try [("system.raw", false), ("data.raw", false), ("seed.iso", true)].map { name, readOnly in
            let attachment = try VZDiskImageStorageDeviceAttachment(url: directory.appendingPathComponent(name), readOnly: readOnly)
            return VZVirtioBlockDeviceConfiguration(attachment: attachment)
        }
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
        let output = Pipe()
        consolePipe = output
        let input = FileHandle(forReadingAtPath: "/dev/null")!
        serial.attachment = VZFileHandleSerialPortAttachment(fileHandleForReading: input, fileHandleForWriting: output.fileHandleForWriting)
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { handle.readabilityHandler = nil; return }
            self?.consoleQueue.sync { [weak self] in self?.logConsole(data) }
        }
        configuration.serialPorts = [serial]
        try configuration.validate()
        vm = VZVirtualMachine(configuration: configuration)
        vm!.delegate = self
        if let device = vm!.socketDevices.first as? VZVirtioSocketDevice {
            let listener = VZVirtioSocketListener()
            let delegate = HostExecListener(host: self)
            listener.delegate = delegate
            hostExecListener = listener
            hostExecDelegate = delegate
            device.setSocketListener(listener, forPort: 4051)
        }
        try listen(guest: false)
        try listen(guest: true)
        listenShare()
        vm!.start { [self] result in
            switch result {
            case .success:
                if phase == "starting" { phase = "running" }
                else { vm?.stop(completionHandler: { _ in }) }
            case .failure(let error): phase = "failed"; failure = error.localizedDescription
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 120) { [self] in
            if phase == "starting" { phase = "failed"; failure = "VM boot exceeded 120 seconds."; vm?.stop(completionHandler: { _ in }) }
        }
    }
    func logConsole(_ data: Data) {
        let url = directory.appendingPathComponent("console.log")
        let previous = (try? Data(contentsOf: url)) ?? Data()
        let combined = previous + data
        try? Data(combined.suffix(1024 * 1024)).write(to: url, options: .atomic)
        chmod(url.path, 0o600)
    }
    func listen(guest: Bool) throws {
        let path = directory.appendingPathComponent(guest ? "guest.sock" : "control.sock").path
        var addr = try address(path)
        unlink(path)
        let listener = socket(AF_UNIX, SOCK_STREAM, 0)
        guard listener >= 0 else { try fail("Cannot create the VM control socket.") }
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(listener, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard bound == 0, chmod(path, 0o600) == 0, Darwin.listen(listener, 64) == 0 else {
            try fail("Cannot bind the VM control socket: \(String(cString: strerror(errno))).")
        }
        DispatchQueue.global().async { [self] in
            while true {
                let client = accept(listener, nil, nil)
                if client < 0 { if errno == EINTR { continue }; break }
                guard channels.wait(timeout: .now()) == .success else { close(client); continue }
                var uid: uid_t = 0, gid: gid_t = 0
                guard getpeereid(client, &uid, &gid) == 0, uid == getuid() else { close(client); channels.signal(); continue }
                var timeout = timeval(tv_sec: 30, tv_usec: 0)
                setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
                setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
                var noPipe: Int32 = 1
                setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noPipe, 4)
                if guest {
                    DispatchQueue.main.async { [self] in connectGuest(client, port: 4050, slots: channels) }
                } else {
                    DispatchQueue.global().async { [self] in handle(client) }
                }
            }
        }
    }
    func handle(_ fd: Int32) {
        var line = Data(), byte: UInt8 = 0
        let deadline = DispatchTime.now().uptimeNanoseconds + 30_000_000_000
        var terminated = false
        while line.count < 65536 && DispatchTime.now().uptimeNanoseconds < deadline {
            var pending = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            let remaining = deadline - min(deadline, DispatchTime.now().uptimeNanoseconds)
            let ready = poll(&pending, 1, Int32(min(30_000, remaining / 1_000_000)))
            if ready < 0 && errno == EINTR { continue }
            if ready <= 0 { break }
            let count = read(fd, &byte, 1)
            if count != 1 { close(fd); channels.signal(); return }
            if byte == 10 { terminated = true; break }
            line.append(byte)
        }
        guard terminated, line.count < 65536,
              let request = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
              let id = request["id"], let method = request["method"] as? String else {
            reply(fd, ["error": ["message": "Invalid VM control request."]]); close(fd); channels.signal(); return
        }
        DispatchQueue.main.async { [self] in
            defer { channels.signal() }
            switch method {
            case "host.status":
                let share: [String: Any] = ["state": sharePort == nil ? "unavailable" : "listening",
                                            "port": sharePort ?? NSNull(), "error": shareError ?? NSNull()]
                let result: [String: Any] = ["state": phase, "pid": getpid(), "error": failure ?? NSNull(),
                                             "settings": config, "share": share]
                reply(fd, ["id": id, "result": result]); close(fd)
            case "host.stop":
                guard let machine = vm, machine.canStop else {
                    reply(fd, ["id": id, "result": ["state": "stopped"]]); close(fd); exit(0)
                }
                phase = "stopping"
                // Graceful ACPI shutdown first; forced stop has a fixed deadline.
                try? machine.requestStop()
                DispatchQueue.main.asyncAfter(deadline: .now() + 20) { [self] in
                    if phase != "stopped" { machine.stop { _ in exit(0) } }
                }
                DispatchQueue.main.asyncAfter(deadline: .now() + 30) { exit(1) }
                reply(fd, ["id": id, "result": ["state": phase]]); close(fd)
            default: reply(fd, ["id": id, "error": ["message": "Unknown VM control method."]]); close(fd)
            }
        }
    }
    func connectGuest(_ fd: Int32, port: UInt32, slots: DispatchSemaphore) {
        guard phase == "running", let socketDevice = vm?.socketDevices.first as? VZVirtioSocketDevice else { close(fd); slots.signal(); return }
        var completed = false
        DispatchQueue.main.asyncAfter(deadline: .now() + 15) {
            if !completed { completed = true; close(fd); slots.signal() }
        }
        socketDevice.connect(toPort: port) { [self] result in
            if completed { if case .success(let connection) = result { connection.close() }; return }
            completed = true
            switch result {
            case .failure: close(fd); slots.signal()
            case .success(let connection):
                var timeout = timeval(tv_sec: 30, tv_usec: 0)
                setsockopt(connection.fileDescriptor, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
                DispatchQueue.global().async { [self] in bridge(fd, connection, slots: slots) }
            }
        }
    }
    func listenShare() {
        // The host saves the port once; a taken port falls back to a free one, and
        // host.status reports the port in use.
        let settings = directory.appendingPathComponent("share-bridge.json")
        guard let data = try? Data(contentsOf: settings),
              let value = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let requested = value["port"] as? Int, (1024...65535).contains(requested) else {
            shareError = "The share bridge port is not configured."; return
        }
        let listener = socket(AF_INET, SOCK_STREAM, 0)
        guard listener >= 0 else { shareError = "Cannot create the share bridge socket."; return }
        var reuse: Int32 = 1
        setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &reuse, socklen_t(MemoryLayout<Int32>.size))
        func bindLoopback(_ port: Int) -> Bool {
            var addr = sockaddr_in()
            addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
            addr.sin_family = sa_family_t(AF_INET)
            addr.sin_port = in_port_t(UInt16(port).bigEndian)
            addr.sin_addr = in_addr(s_addr: INADDR_LOOPBACK.bigEndian)
            return withUnsafePointer(to: &addr) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(listener, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
            } == 0
        }
        guard bindLoopback(requested) || (errno == EADDRINUSE && bindLoopback(0)), Darwin.listen(listener, 16) == 0 else {
            shareError = "Cannot bind the share bridge: \(String(cString: strerror(errno)))."; close(listener); return
        }
        var bound = sockaddr_in()
        var length = socklen_t(MemoryLayout<sockaddr_in>.size)
        let named = withUnsafeMutablePointer(to: &bound) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(listener, $0, &length) }
        }
        guard named == 0 else { shareError = "Cannot read the share bridge port."; close(listener); return }
        sharePort = Int(UInt16(bigEndian: bound.sin_port))
        DispatchQueue.global().async { [self] in
            while true {
                let client = accept(listener, nil, nil)
                if client < 0 { if errno == EINTR { continue }; break }
                guard shareChannels.wait(timeout: .now()) == .success else { close(client); continue }
                var noPipe: Int32 = 1
                setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noPipe, 4)
                DispatchQueue.main.async { [self] in connectGuest(client, port: 4052, slots: shareChannels) }
            }
        }
    }
    func connectHostExec(_ connection: VZVirtioSocketConnection) {
        let client = socket(AF_UNIX, SOCK_STREAM, 0)
        guard client >= 0 else { connection.close(); channels.signal(); return }
        do {
            var addr = try address(directory.appendingPathComponent("host-exec.sock").path)
            let connected = withUnsafePointer(to: &addr) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    Darwin.connect(client, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            guard connected == 0 else { close(client); connection.close(); channels.signal(); return }
            var timeout = timeval(tv_sec: 30, tv_usec: 0)
            setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            setsockopt(connection.fileDescriptor, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            bridge(client, connection, slots: channels)
        } catch { close(client); connection.close(); channels.signal() }
    }
    func bridge(_ client: Int32, _ connection: VZVirtioSocketConnection, slots: DispatchSemaphore) {
        defer { close(client); connection.close(); slots.signal() }
        let guest = connection.fileDescriptor
        var descriptors = [pollfd(fd: client, events: Int16(POLLIN), revents: 0), pollfd(fd: guest, events: Int16(POLLIN), revents: 0)]
        var bytes = [UInt8](repeating: 0, count: 65536)
        while true {
            let ready = poll(&descriptors, 2, 3600000)
            if ready < 0 && errno == EINTR { continue }
            if ready <= 0 { break }
            for index in 0..<2 where descriptors[index].revents != 0 {
                if descriptors[index].revents & Int16(POLLIN) == 0 { return }
                let count = read(descriptors[index].fd, &bytes, bytes.count)
                if count <= 0 { return }
                if !writeAll(descriptors[1-index].fd, Data(bytes.prefix(count))) { return }
            }
        }
    }
    func guestDidStop(_ virtualMachine: VZVirtualMachine) { phase = "stopped"; exit(0) }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) { phase = "failed"; failure = error.localizedDescription }
}

signal(SIGPIPE, SIG_IGN)
if CommandLine.arguments == [CommandLine.arguments[0], "--check"] {
    print(String(data: json(["supported": VZVirtualMachine.isSupported, "architecture": "arm64", "protocol": 1]), encoding: .utf8)!)
    exit(VZVirtualMachine.isSupported ? 0 : 1)
}
guard CommandLine.arguments.count == 3, CommandLine.arguments[1] == "serve" else {
    fputs("Use studio-linux-vm serve STATE_DIRECTORY or --check.\n", stderr); exit(2)
}
let host = Host(directory: URL(fileURLWithPath: CommandLine.arguments[2]).standardizedFileURL)
do { try host.boot(); RunLoop.main.run() }
catch { fputs("Linux VM helper: \(error.localizedDescription)\n", stderr); exit(1) }
