import SwiftUI

@main
struct GooseSwiftApp: App {
  @Environment(\.scenePhase) private var scenePhase
  @StateObject private var model = GooseAppModel()
  @StateObject private var router = AppRouter()
  @AppStorage(Forme.appearanceKey) private var appearance = "system"

  init() {
    GooseTheme.configureAppearance()
    Forme.registerFonts()
  }

  var body: some Scene {
    WindowGroup {
      RootView()
        .environmentObject(model)
        .environmentObject(model.packetMonitor)
        .environmentObject(model.ble.messageStore)
        .environmentObject(router)
        .onAppear { Forme.applyAppearance(appearance) }
        .onChange(of: appearance) { _, v in Forme.applyAppearance(v) }
        .onOpenURL { url in
          if model.handleDebugCommandDeepLink(url) {
            router.openMore(nil)
          } else {
            _ = router.handleDeepLink(url)
          }
        }
        .onChange(of: scenePhase) { _, phase in
          switch phase {
          case .active:
            model.handleAppLifecycleChange("active")
          case .inactive:
            model.handleAppLifecycleChange("inactive")
          case .background:
            model.handleAppLifecycleChange("background")
          @unknown default:
            model.handleAppLifecycleChange("unknown")
          }
        }
    }
  }
}
