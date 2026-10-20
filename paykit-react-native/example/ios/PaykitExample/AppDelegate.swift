import UIKit
import React
import React_RCTAppDelegate
import ReactAppDependencyProvider

@main
class AppDelegate: RCTAppDelegate {
  /// Smoke harness launch options. The keys match LAUNCH_KEYS in
  /// src/launchConfig.ts. Pass them as launch arguments, which iOS exposes
  /// through UserDefaults, for example
  /// `xcrun simctl launch booted com.synonym.paykit.rnexample -paykitMode local -paykitAutorun true`.
  private static let launchKeys = [
    "paykitMode",
    "paykitAutorun",
    "paykitHomeserver",
    "paykitLocalHost",
    "paykitSignupTokenA",
    "paykitSignupTokenB",
  ]

  override func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey : Any]? = nil) -> Bool {
    self.moduleName = "PaykitExample"
    self.dependencyProvider = RCTAppDependencyProvider()

    var props: [String: String] = [:]
    for key in Self.launchKeys {
      if let value = UserDefaults.standard.string(forKey: key) {
        props[key] = value
      }
    }
    self.initialProps = props

    // Release builds log only errors by default. The smoke harness reports
    // through console.log (SMOKE_STEP / SMOKE_RESULT lines), so keep info.
    RCTSetLogThreshold(.info)

    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }

  override func sourceURL(for bridge: RCTBridge) -> URL? {
    self.bundleURL()
  }

  override func bundleURL() -> URL? {
#if DEBUG
    RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: "index")
#else
    Bundle.main.url(forResource: "main", withExtension: "jsbundle")
#endif
  }
}
