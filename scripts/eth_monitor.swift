import Foundation
import SystemConfiguration

let interfaceName = "en5"
let targetScript = "/Users/kartinul/Developer/github/kartinul/dotfiles/scripts/extend.sh"
var process: Process? = nil
var lastState: Bool? = nil

func log(_ message: String) {
    let formatter = DateFormatter()
    formatter.dateFormat = "yyyy-MM-dd HH:mm:ss"
    let timestamp = formatter.string(from: Date())
    print("[\(timestamp)] \(message)")
    fflush(stdout)
}

func isInterfaceActive() -> Bool {
    guard let store = SCDynamicStoreCreate(nil, "EthMonitor" as CFString, nil, nil) else {
        return false
    }
    let key = "State:/Network/Interface/\(interfaceName)/Link" as CFString
    if let dict = SCDynamicStoreCopyValue(store, key) as? [String: Any],
       let active = dict["Active"] as? Bool {
        return active
    }
    return false
}

func syncState() {
    let active = isInterfaceActive()
    
    // Prevent duplicate logs if SCDynamicStore sends multiple notifications for one event
    if active == lastState { return }
    lastState = active

    if active {
        log("ETHERNET PLUGGED IN (\(interfaceName))")
        if process == nil || !process!.isRunning {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: targetScript)
            do {
                try p.run()
                process = p
                log("Started \(targetScript) [PID: \(p.processIdentifier)]")
            } catch {
                log("Failed to start script: \(error)")
            }
        }
    } else {
        log("ETHERNET UNPLUGGED (\(interfaceName))")
        if let p = process, p.isRunning {
            p.terminate()
            process = nil
            log("Terminated \(targetScript)")
        }
    }
}

var context = SCDynamicStoreContext(version: 0, info: nil, retain: nil, release: nil, copyDescription: nil)
guard let store = SCDynamicStoreCreate(nil, "EthMonitor" as CFString, { _, _, _ in
    syncState()
}, &context) else {
    log("Failed to initialize SystemConfiguration store")
    exit(1)
}

let pattern = "State:/Network/Interface/\(interfaceName)/.*" as CFString
SCDynamicStoreSetNotificationKeys(store, nil, [pattern] as CFArray)

if let source = SCDynamicStoreCreateRunLoopSource(nil, store, 0) {
    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .defaultMode)
}

log("Started monitoring \(interfaceName) link status...")

// Initial state check
syncState()

// Event loop
CFRunLoopRun()
