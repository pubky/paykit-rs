import UIKit
import React
import React_RCTAppDelegate
import ReactAppDependencyProvider

@main
class AppDelegate: UIResponder, UIApplicationDelegate {
  var window: UIWindow?

  var reactNativeDelegate: ReactNativeDelegate?
  var reactNativeFactory: RCTReactNativeFactory?

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

  func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
  ) -> Bool {
    let delegate = ReactNativeDelegate()
    let factory = RCTReactNativeFactory(delegate: delegate)
    delegate.dependencyProvider = RCTAppDependencyProvider()

    reactNativeDelegate = delegate
    reactNativeFactory = factory

    var props: [String: String] = [:]
    for key in Self.launchKeys {
      if let value = UserDefaults.standard.string(forKey: key) {
        props[key] = value
      }
    }

    // Release builds log only errors by default. The smoke harness reports
    // through console.log (SMOKE_STEP / SMOKE_RESULT lines), so keep info.
    RCTSetLogThreshold(.info)

    window = UIWindow(frame: UIScreen.main.bounds)

    factory.startReactNative(
      withModuleName: "PaykitExample",
      in: window,
      initialProperties: props,
      launchOptions: launchOptions
    )

    return true
  }
}

class ReactNativeDelegate: RCTDefaultReactNativeFactoryDelegate {
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
