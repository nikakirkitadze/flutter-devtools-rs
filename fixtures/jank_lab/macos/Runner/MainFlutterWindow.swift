import Cocoa
import FlutterMacOS

class MainFlutterWindow: NSWindow {
  override func awakeFromNib() {
    let flutterViewController = FlutterViewController()
    let windowFrame = self.frame
    self.contentViewController = flutterViewController
    self.setFrame(windowFrame, display: true)

    // A window that is covered draws no frames, so automated recordings set JANK_LAB_FLOAT=1
    // to keep it above other windows.
    if ProcessInfo.processInfo.environment["JANK_LAB_FLOAT"] == "1" {
      self.level = .floating
    }

    RegisterGeneratedPlugins(registry: flutterViewController)

    super.awakeFromNib()
  }
}
