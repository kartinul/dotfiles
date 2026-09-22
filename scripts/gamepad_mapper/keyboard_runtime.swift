import CoreGraphics
import Dispatch
import Foundation
import IOKit.hid

private func configuredValue(_ name: String) -> String? {
    if let value = ProcessInfo.processInfo.environment[name] {
        return value
    }
    guard let contents = try? String(contentsOfFile: ".env", encoding: .utf8) else {
        return nil
    }
    for line in contents.split(whereSeparator: \.isNewline) {
        let text = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !text.hasPrefix("#"), let separator = text.firstIndex(of: "=") else {
            continue
        }
        let key = text[..<separator].trimmingCharacters(in: .whitespaces)
        guard key == name else { continue }
        return text[text.index(after: separator)...]
            .trimmingCharacters(in: .whitespaces)
            .trimmingCharacters(in: CharacterSet(charactersIn: "\"'"))
    }
    return nil
}

private func configuredInt(_ name: String, default defaultValue: Int) -> Int {
    guard let value = configuredValue(name) else { return defaultValue }
    if value.lowercased().hasPrefix("0x") {
        return Int(value.dropFirst(2), radix: 16) ?? defaultValue
    }
    return Int(value) ?? defaultValue
}

private struct Mapping {
    let type: String
    let byte: Int
    let bit: Int
    let inverted: Bool
}

private struct Binding {
    let key: String?
    let mode: String
    let direction: String
    let threshold: Double
    let directions: [String: String]
}

private final class Runtime {
    private var manager: IOHIDManager!
    private var mappings: [String: Mapping] = [:]
    private var bindings: [String: Binding] = [:]
    private var pressedKeys: Set<CGKeyCode> = []
    private var lastValues: [String: Double] = [:]
    private var moveCursorBeforeKeyboardInput = false
    private var shutdownSource: DispatchSourceSignal?
    private var vendorID: Int { configuredInt("HID_VENDOR_ID", default: 0x2563) }
    private var productID: Int { configuredInt("HID_PRODUCT_ID", default: 0x0575) }
    private var configPath: String { configuredValue("PAD_CONFIG_FILE") ?? "controller_config.json" }

    func start() throws {
        try loadConfig()
        printConfigSummary()

        manager = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
        let matching: [String: Any] = [
            kIOHIDVendorIDKey as String: vendorID,
            kIOHIDProductIDKey as String: productID
        ]
        IOHIDManagerSetDeviceMatching(manager, matching as CFDictionary)

        let context = Unmanaged.passUnretained(self).toOpaque()
        IOHIDManagerRegisterInputReportCallback(
            manager,
            { context, _, _, _, _, report, length in
                guard let context else { return }
                let runtime = Unmanaged<Runtime>.fromOpaque(context).takeUnretainedValue()
                runtime.handle(report: report, length: Int(length))
            },
            context
        )
        IOHIDManagerScheduleWithRunLoop(
            manager,
            CFRunLoopGetCurrent(),
            CFRunLoopMode.defaultMode.rawValue
        )

        let result = IOHIDManagerOpen(manager, IOOptionBits(kIOHIDOptionsTypeNone))
        guard result == kIOReturnSuccess else {
            throw NSError(domain: "Runtime", code: Int(result), userInfo: [
                NSLocalizedDescriptionKey: "Could not open the HID device (IOReturn \(result))."
            ])
        }

        print("Running native HID keyboard emulation. Press Ctrl+C to stop.")
        installShutdownHandler()
        CFRunLoopRun()
        releaseAllKeys()
        IOHIDManagerUnscheduleFromRunLoop(manager, CFRunLoopGetCurrent(), CFRunLoopMode.defaultMode.rawValue)
        IOHIDManagerClose(manager, IOOptionBits(kIOHIDOptionsTypeNone))
    }

    private func loadConfig() throws {
        let url = URL(fileURLWithPath: configPath)
        let object = try JSONSerialization.jsonObject(with: Data(contentsOf: url))
        guard let root = object as? [String: Any] else {
            throw NSError(domain: "Runtime", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid config root."])
        }

        guard let mappingObject = root["mapping"] as? [String: Any] else {
            throw NSError(domain: "Runtime", code: 2, userInfo: [NSLocalizedDescriptionKey: "Config has no mapping."])
        }
        for (name, value) in mappingObject {
            guard let item = value as? [String: Any],
                  let type = item["type"] as? String,
                  let byte = item["byte"] as? Int else { continue }
            mappings[name] = Mapping(
                type: type,
                byte: byte,
                bit: item["bit"] as? Int ?? 0,
                inverted: item["inverted"] as? Bool ?? false
            )
        }

        var keybindObject = root["keybinds"] as? [String: Any]
        if let activeGame = root["active_game"] as? String,
           let games = root["games"] as? [String: Any],
           let game = games[activeGame] as? [String: Any],
           let gameKeybinds = game["keybinds"] as? [String: Any] {
            keybindObject = gameKeybinds
            if let cursor = game["cursor"] as? [String: Any],
               cursor["move_before_keyboard_input"] as? Bool == true,
               cursor["position"] as? String == "right_middle" {
                moveCursorBeforeKeyboardInput = true
            }
        }
        guard let keybindObject else { return }

        for (name, value) in keybindObject {
            if let key = value as? String {
                bindings[name] = Binding(
                    key: key, mode: "button", direction: "positive",
                    threshold: 0.5, directions: [:]
                )
                continue
            }
            guard let item = value as? [String: Any] else { continue }
            let directions = item["directions"] as? [String: String] ?? [:]
            bindings[name] = Binding(
                key: item["key"] as? String,
                mode: item["mode"] as? String ?? "button",
                direction: item["direction"] as? String ?? "positive",
                threshold: item["threshold"] as? Double ?? 0.5,
                directions: directions
            )
        }
    }

    private func printConfigSummary() {
        var lines: [(String, String)] = []

        for (name, binding) in bindings {
            if let key = binding.key {
                lines.append((key, name))
                continue
            }

            let directions = binding.directions.keys.sorted { lhs, rhs in
                let lhsOrder = lhs == "positive" ? 0 : 1
                let rhsOrder = rhs == "positive" ? 0 : 1
                return lhsOrder < rhsOrder
            }
            for direction in directions {
                guard let key = binding.directions[direction] else { continue }
                let arrow: String
                if name.hasSuffix("_Y") {
                    arrow = direction == "positive" ? "↑" : "↓"
                } else {
                    arrow = direction == "positive" ? "→" : "←"
                }
                lines.append((key, name + " " + arrow))
            }
        }

        if lines.isEmpty {
            print("Keyboard bindings: (none configured)")
            return
        }

        let sortedLines = lines.sorted { lhs, rhs in
            if lhs.1 == rhs.1 { return lhs.0 < rhs.0 }
            return lhs.1 < rhs.1
        }
        let controllerWidth = max("Controller".count, sortedLines.map { $0.1.count }.max() ?? 0)
        let keyWidth = max("Keyboard Key".count, sortedLines.map { $0.0.count }.max() ?? 0)
        let border = "+-\(String(repeating: "-", count: controllerWidth))-+-\(String(repeating: "-", count: keyWidth))-+"

        print("Keyboard bindings")
        print(border)
        print("| \("Controller".padding(toLength: controllerWidth, withPad: " ", startingAt: 0)) | \("Keyboard Key".padding(toLength: keyWidth, withPad: " ", startingAt: 0)) |")
        print(border)
        for (key, controller) in sortedLines {
            print("| \(controller.padding(toLength: controllerWidth, withPad: " ", startingAt: 0)) | \(key.padding(toLength: keyWidth, withPad: " ", startingAt: 0)) |")
        }
        print(border)
    }

    private func handle(report: UnsafeMutablePointer<UInt8>, length: Int) {
        let count = length

        for (name, binding) in bindings {
            guard let mapping = mappings[name], mapping.byte < count else { continue }
            let raw = report[mapping.byte]
            let value = value(for: name, mapping: mapping, raw: raw)

            if binding.mode == "directions" {
                for (direction, key) in binding.directions {
                    let active = direction == "positive"
                        ? value >= binding.threshold
                        : value <= -binding.threshold
                    update(key: key, active: active, stateKey: "\(name):\(direction)")
                }
            } else {
                let active: Bool
                if mapping.type == "button" {
                    active = value > 0.5
                } else if binding.direction == "negative" {
                    active = value <= -binding.threshold
                } else {
                    active = value >= binding.threshold
                }
                if let key = binding.key {
                    update(key: key, active: active, stateKey: name)
                }
            }
        }
    }

    private func value(for name: String, mapping: Mapping, raw: UInt8) -> Double {
        if mapping.type == "button" {
            return (raw & (1 << mapping.bit)) == 0 ? 0 : 1
        }
        if name == "LT" || name == "RT" {
            return Double(raw) / 255.0
        }
        var normalized = (Double(raw) - 127.5) / 127.5
        if abs(normalized) < 0.08 { normalized = 0 }
        if mapping.inverted { normalized = -normalized }
        return max(-1, min(1, normalized))
    }

    private func update(key: String, active: Bool, stateKey: String) {
        let previous = lastValues[stateKey] ?? 0
        let wasActive = previous > 0.5
        lastValues[stateKey] = active ? 1 : 0

        if key == "mouse_wheel_down" || key == "mouse_wheel_up" {
            guard active && !wasActive else { return }
            postScroll(delta: key == "mouse_wheel_down" ? -1 : 1)
            return
        }

        guard active != wasActive else { return }

        guard let keyCode = keyCode(for: key) else { return }
        if active {
            guard !pressedKeys.contains(keyCode) else { return }
            pressedKeys.insert(keyCode)
            post(keyCode: keyCode, down: true)
        } else {
            guard pressedKeys.contains(keyCode) else { return }
            pressedKeys.remove(keyCode)
            post(keyCode: keyCode, down: false)
        }
    }

    private func post(keyCode: CGKeyCode, down: Bool) {
        if moveCursorBeforeKeyboardInput {
            moveCursorToRightMiddle()
        }
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: keyCode, keyDown: down) else { return }
        event.post(tap: .cghidEventTap)
    }

    private func moveCursorToRightMiddle() {
        let bounds = CGDisplayBounds(CGMainDisplayID())
        let position = CGPoint(x: bounds.maxX - 1, y: bounds.midY)
        CGWarpMouseCursorPosition(position)
    }

    private func postScroll(delta: Int32) {
        guard let event = CGEvent(
            scrollWheelEvent2Source: nil,
            units: .line,
            wheelCount: 1,
            wheel1: delta,
            wheel2: 0,
            wheel3: 0
        ) else { return }
        event.post(tap: .cghidEventTap)
    }

    private func keyCode(for value: String) -> CGKeyCode? {
        let key = value.lowercased()
        let codes: [String: CGKeyCode] = [
            "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5,
            "z": 6, "x": 7, "c": 8, "v": 9, "b": 11, "q": 12,
            "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
            "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23,
            "=": 24, "9": 25, "7": 26, "-": 27, "8": 28, "0": 29,
            "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35,
            "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42,
            ",": 43, "/": 44, "n": 45, "m": 46, ".": 47,
            "tab": 48, "space": 49, "delete": 51, "enter": 36, "return": 36,
            "esc": 53, "escape": 53, "left": 123, "right": 124,
            "down": 125, "up": 126, "shift": 56, "ctrl": 59, "control": 59,
            "alt": 58, "option": 58, "cmd": 55, "command": 55
        ]
        return codes[key]
    }

    private func installShutdownHandler() {
        signal(SIGINT, SIG_IGN)
        let runLoop = CFRunLoopGetCurrent()
        let source = DispatchSource.makeSignalSource(signal: SIGINT, queue: .global(qos: .userInteractive))
        source.setEventHandler { [weak self] in
            self?.releaseAllKeys()
            CFRunLoopStop(runLoop)
        }
        shutdownSource = source
        source.resume()
    }

    private func releaseAllKeys() {
        for keyCode in pressedKeys { post(keyCode: keyCode, down: false) }
        pressedKeys.removeAll()
    }
}

private let runtime = Runtime()
do {
    try runtime.start()
} catch {
    fputs("Error: \(error.localizedDescription)\n", stderr)
    exit(1)
}