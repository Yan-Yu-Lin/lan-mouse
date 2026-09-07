import AppKit
import Darwin

// A persistent, click-through indicator. --notify never waits for the UI process.
let directory = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".local/state/lan-mouse")
let fifo = directory.appendingPathComponent("hud.fifo").path
let states: Set<String> = ["connecting", "remote", "local", "error", "reset"]
if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--notify" {
    let state = CommandLine.arguments[2]
    guard states.contains(state) else { exit(2) }
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
    let label = NSTextField(labelWithString: "")
    let symbol = NSImageView()
    var pending: DispatchWorkItem?
    var state = "reset"
    let sounds = ProcessInfo.processInfo.environment["LAN_MOUSE_SOUND"] == "1"

    init() {
        panel.level = .statusBar
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle, .fullScreenAuxiliary]
        let background = NSVisualEffectView()
        background.material = .hudWindow
        background.blendingMode = .behindWindow
        background.state = .active
        background.wantsLayer = true
        background.layer?.cornerRadius = 16
        background.layer?.masksToBounds = true
        panel.contentView = background
        label.font = .systemFont(ofSize: 15, weight: .medium)
        label.textColor = .labelColor
        background.addSubview(label)
        background.addSubview(symbol)
        symbol.contentTintColor = .systemTeal
    }

    func draw(_ text: String, icon: String, compact: Bool = false) {
        guard let screen = NSScreen.main ?? NSScreen.screens.first else { return }
        let width: CGFloat = compact ? 138 : 272
        let height: CGFloat = compact ? 34 : 58
        let visible = screen.visibleFrame
        let x = compact ? visible.maxX - width - 18 : visible.midX - width / 2
        panel.setFrame(NSRect(x: x, y: visible.maxY - height - 22, width: width, height: height), display: true)
        symbol.image = NSImage(systemSymbolName: icon, accessibilityDescription: text)
        symbol.frame = NSRect(x: compact ? 12 : 20, y: (height-22)/2, width: 22, height: 22)
        label.stringValue = text
        label.font = .systemFont(ofSize: compact ? 12 : 15, weight: .medium)
        label.frame = NSRect(x: compact ? 42 : 56, y: (height-22)/2, width: width-62, height: 22)
        panel.alphaValue = 1
        panel.orderFrontRegardless()
    }

    func update(_ next: String) {
        guard states.contains(next) else { return }
        pending?.cancel()
        state = next
        switch next {
        case "connecting":
            let task = DispatchWorkItem { [weak self] in self?.draw("Connecting to Omarchy…", icon: "arrow.right.circle") }
            pending = task
            DispatchQueue.main.asyncAfter(deadline: .now()+0.25, execute: task)
            return
        case "remote": draw("Controlling Omarchy", icon: "desktopcomputer")
        case "local": draw("Controlling Mac", icon: "laptopcomputer")
        case "error": draw("Back on Mac · switch failed", icon: "exclamationmark.circle")
        default: panel.orderOut(nil); return
        }
        if sounds && (next == "remote" || next == "local") {
            NSSound(named: next == "remote" ? "Pop" : "Tink")?.play()
        }
        let task = DispatchWorkItem { [weak self] in
            guard let self = self else { return }
            if self.state == "remote" {
                self.draw("Omarchy", icon: "desktopcomputer", compact: true)
            } else {
                NSAnimationContext.runAnimationGroup { context in
                    context.duration = 0.18
                    self.panel.animator().alphaValue = 0
                }
            }
        }
        pending = task
        DispatchQueue.main.asyncAfter(deadline: .now()+(next == "error" ? 2.5 : 1.1), execute: task)
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
if CommandLine.arguments.contains("--preview") { hud.update("remote") }
if let index = CommandLine.arguments.firstIndex(of: "--snapshot"), index + 1 < CommandLine.arguments.count {
    hud.update("remote")
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
