import AppKit
import Darwin

// A persistent, click-through indicator. --notify never waits for the UI process.
let directory = ProcessInfo.processInfo.environment["LAN_MOUSE_HUD_DIR"].map { URL(fileURLWithPath: $0) }
    ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".local/state/lan-mouse")
let stateFile = directory.appendingPathComponent("focus-state")
let fifo = directory.appendingPathComponent("hud.fifo").path
let states: Set<String> = ["connecting", "remote", "local", "error", "reset"]
if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--notify" {
    let state = CommandLine.arguments[2]
    guard states.contains(state) else { exit(2) }
    if state != "connecting" {
        let stable = state == "remote" ? "remote" : "local"
        try? Data(stable.utf8).write(to: stateFile, options: .atomic)
    }
    let fd = open(fifo, O_WRONLY | O_NONBLOCK | O_NOFOLLOW)
    if fd >= 0 {
        signal(SIGPIPE, SIG_IGN)
        let bytes = Array((state + "\n").utf8)
        bytes.withUnsafeBytes { _ = write(fd, $0.baseAddress, $0.count) }
        close(fd)
    }
    exit(0)
}

final class Overlay: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

final class HUD {
    let panel = Overlay(contentRect: .zero, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
    let background = NSView()
    let symbol = NSImageView()
    var remote = false
    let sounds = ProcessInfo.processInfo.environment["LAN_MOUSE_SOUND"] != "0"
    let macSound = NSSound(named: "Tink")
    let linuxSound = NSSound(named: "Pop")

    init() {
        panel.level = .statusBar
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle, .fullScreenAuxiliary]
        background.wantsLayer = true
        background.layer?.cornerRadius = 20
        background.layer?.masksToBounds = true
        panel.contentView = background
        background.addSubview(symbol)
        symbol.contentTintColor = .white
        symbol.frame = NSRect(x: 24, y: 14, width: 48, height: 40)
        macSound?.volume = 0.55
        linuxSound?.volume = 0.55
    }

    func draw() {
        guard let screen = NSScreen.main ?? NSScreen.screens.first else { return }
        let visible = screen.frame
        panel.setFrame(NSRect(x: visible.midX - 48, y: visible.midY - 34, width: 96, height: 68), display: true)
        // One solid hue per destination, with the same white keyboard glyph.
        let color = remote
            ? NSColor(srgbRed: 0.76, green: 0.20, blue: 0.23, alpha: 1)
            : NSColor(srgbRed: 0.13, green: 0.49, blue: 0.29, alpha: 1)
        background.layer?.backgroundColor = color.cgColor
        symbol.image = NSImage(systemSymbolName: "keyboard", accessibilityDescription: remote ? "Controlling Linux" : "Controlling Mac")
        panel.orderFrontRegardless()
    }

    func update(_ next: String, announce: Bool = true) {
        guard states.contains(next), next != "connecting" else { return }
        let changed = remote != (next == "remote")
        remote = next == "remote"
        draw()
        if changed && sounds && announce && (next == "remote" || next == "local") {
            let sound = remote ? linuxSound : macSound
            sound?.stop()
            sound?.play()
        }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let hud = HUD()
try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
var info = stat()
if lstat(fifo, &info) == 0 {
    guard info.st_mode & S_IFMT == S_IFIFO && info.st_uid == getuid() else { fatalError("Unexpected HUD pipe") }
} else {
    guard mkfifo(fifo, 0o600) == 0 else { fatalError("Could not create HUD pipe") }
}
let fd = open(fifo, O_RDWR | O_NONBLOCK | O_NOFOLLOW)
guard fd >= 0 else { fatalError("Could not open HUD pipe") }
let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: .main)
var input = ""
source.setEventHandler {
    var buffer = [UInt8](repeating: 0, count: 1024)
    let count = read(fd, &buffer, buffer.count)
    guard count > 0 else { return }
    input += String(decoding: buffer.prefix(count), as: UTF8.self)
    while let newline = input.firstIndex(of: "\n") {
        let command = String(input[..<newline])
        input.removeSubrange(...newline)
        hud.update(command)
    }
    if input.count > 4096 { input = "" }
}
source.resume()
let savedState = (try? String(contentsOf: stateFile, encoding: .utf8)) ?? "local"
hud.update(savedState, announce: false)
if CommandLine.arguments.contains("--preview") { hud.update("remote") }
if let index = CommandLine.arguments.firstIndex(of: "--snapshot"), index + 1 < CommandLine.arguments.count {
    let previewState = CommandLine.arguments.contains("--local") ? "local" : "remote"
    hud.update(previewState, announce: false)
    DispatchQueue.main.asyncAfter(deadline: .now()+0.3) {
        let view = hud.panel.contentView!
        if let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds) {
            view.cacheDisplay(in: view.bounds, to: bitmap)
            try? bitmap.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: CommandLine.arguments[index+1]))
        }
        app.terminate(nil)
    }
}
app.run()
