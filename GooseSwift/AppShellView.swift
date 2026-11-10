import SwiftUI

struct AppShellView: View {
  @EnvironmentObject private var model: GooseAppModel
  @EnvironmentObject private var router: AppRouter
  @StateObject private var healthStore = HealthDataStore()
  @State private var homeHealthPath: [HealthRoute] = []
  @State private var homeSelectedDate = Date()

  var body: some View {
    TabView(selection: tabSelection) {
      ForEach(GooseAppTab.allCases) { tab in
        tabNavigationStack(for: tab)
        .tabItem {
          Label(tab.title, systemImage: tab.systemImage)
        }
        .tag(tab)
      }
    }
    // whoop-lab: keep packet-derived metrics current instead of computing them once per launch.
    .onAppear {
      healthStore.refreshPacketInputsIfNeeded()
    }
    .onReceive(model.ble.$lastHistoricalSyncCompletedAt.compactMap { $0 }) { _ in
      healthStore.refreshPacketInputsAfterCapture()
      // whoop-lab: new history -> re-run the day analysis (cached, ~50 ms when nothing changed)
      if UIApplication.shared.applicationState != .background {
        model.labResults.refresh(force: true)
      }
    }
    .onReceive(NotificationCenter.default.publisher(for: UIApplication.willEnterForegroundNotification)) { _ in
      healthStore.refreshPacketInputsIfNeeded()
      model.labResults.refresh(force: true)
    }
  }

  private var tabSelection: Binding<GooseAppTab> {
    Binding {
      router.selectedTab
    } set: { newTab in
      if newTab == router.selectedTab {
        router.reselect(newTab)
        return
      }
      router.selectedTab = newTab
      model.recordUIAction("tab.selected", detail: newTab.title)
    }
  }

  @ViewBuilder
  private func tabNavigationStack(for tab: GooseAppTab) -> some View {
    if tab == .home {
      NavigationStack(path: $homeHealthPath) {
        tabContent(for: tab)
          .navigationDestination(for: HealthRoute.self) { route in
            HealthRouteDestinationView(route: route, store: healthStore, selectedDate: $homeSelectedDate)
          }
      }
    } else if tab == .health {
      NavigationStack(path: $router.healthPath) {
        tabContent(for: tab)
      }
    } else if tab == .more {
      NavigationStack(path: $router.morePath) {
        tabContent(for: tab)
      }
    } else {
      NavigationStack {
        tabContent(for: tab)
      }
    }
  }

  @ViewBuilder
  private func tabContent(for tab: GooseAppTab) -> some View {
    switch tab {
    case .home:
      HomeDashboardView(
        healthStore: healthStore,
        labResults: model.labResults,
        selectedDate: $homeSelectedDate,
        openHealthRoute: openHomeHealthRoute
      )
    case .health:
      HealthView(store: healthStore, labResults: model.labResults)
    case .alarm:
      AlarmView(controller: model.smartWake, ble: model.ble)
    case .more:
      MoreView(healthStore: healthStore)
    }
  }

  private func openHomeHealthRoute(_ route: HealthRoute) {
    homeHealthPath = [route]
  }
}

enum GooseAppTab: String, CaseIterable, Identifiable {
  case home
  case health
  case alarm
  case more

  var id: String { rawValue }

  var title: String {
    switch self {
    case .home: "Home"
    case .health: "Health"
    case .alarm: "Alarme"
    case .more: "More"
    }
  }

  var systemImage: String {
    switch self {
    case .home: "house"
    case .health: "heart.text.square"
    case .alarm: "alarm"
    case .more: "ellipsis.circle"
    }
  }

}
