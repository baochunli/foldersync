import AppKit
import Darwin

guard CommandLine.arguments.count == 5,
      let pid = Int32(CommandLine.arguments[2]), pid > 1,
      let app = NSRunningApplication(processIdentifier: pid),
      app.bundleIdentifier == CommandLine.arguments[3],
      app.bundleURL?.path == CommandLine.arguments[4],
      app.isFinishedLaunching else {
    exit(1)
}

switch CommandLine.arguments[1] {
case "ready": exit(0)
case "quit": exit(app.terminate() ? 0 : 1)
default: exit(2)
}
