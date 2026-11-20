import SwiftUI

@main
struct GooseSwiftApp: App {
  @Environment(\.scenePhase) private var scenePhase
  @StateObject private var model = GooseAppModel()
  @StateObject private var router = AppRouter()
  @AppStorage(Forme.appearanceKey) private var appearance = "system"
  /// In the background the view tree is swapped for an empty one (3 s after, once iOS has taken the
  /// app-switcher snapshot): live heart rate, sync counters and sport state publish every second
  /// and SwiftUI kept re-rendering the Today screen nobody sees — 40 % of the app's background CPU
  /// on 20 Nov. Tabs, navigation paths and the profile sheet live in AppRouter and come back as
  /// they were.
  @State private var renderSuspended = false

  init() {
    GooseTheme.configureAppearance()
    Forme.registerFonts()
    DatabaseMaintenance.register()
  }

  var body: some Scene {
    WindowGroup {
      Group {
        if renderSuspended {
          Color.clear
        } else {
          RootView()
        }
      }
        .environmentObject(model)
        .environmentObject(model.packetMonitor)
        .environmentObject(model.ble.messageStore)
        .environmentObject(router)
        .onAppear {
          Forme.applyAppearance(appearance)
          // launched by iOS in the background (Bluetooth restore)
          if UIApplication.shared.applicationState == .background { suspendRenderingSoon() }
        }
        .onChange(of: appearance) { _, v in Forme.applyAppearance(v) }
        .onOpenURL { url in
          if model.handleDebugCommandDeepLink(url) {
            router.openMore(nil)
          } else {
            _ = router.handleDeepLink(url)
          }
        }
        .onChange(of: scenePhase) { _, phase in
          AppForegroundState.set(background: phase == .background)
          if phase == .background {
            suspendRenderingSoon()
          } else {
            renderSuspended = false
          }
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

  private func suspendRenderingSoon() {
    DispatchQueue.main.asyncAfter(deadline: .now() + 3) {
      if UIApplication.shared.applicationState == .background { renderSuspended = true }
    }
  }
}
