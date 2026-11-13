import SwiftUI

/// Forme du jour: four tabs in a floating glass bar plus the red "start an activity" button.
/// Level 1 (glance) is Today; level 2 (context) the Sleep / Effort tabs and the detail screens;
/// level 3 (technical) the collapsible details and Indicators. Profile, device and settings (the
/// former More tab) open from the profile button; the smart wake from the Sleep screen; sport
/// sessions are the Effort screen's "Séances" (one list for detected and started activities).
struct AppShellView: View {
  @EnvironmentObject private var model: GooseAppModel
  @EnvironmentObject private var router: AppRouter
  @StateObject private var healthStore = HealthDataStore()
  @State private var showStart = false
  @State private var showLive = false

  var body: some View {
    ZStack(alignment: .bottom) {
      TabView(selection: tabSelection) {
        NavigationStack(path: $router.todayPath) {
          FormeTodayView(labResults: model.labResults, sport: model.sport)
            .formeDestinations(healthStore: healthStore)
        }
        .tag(GooseAppTab.today)
        .toolbar(.hidden, for: .tabBar)

        NavigationStack(path: $router.sleepPath) {
          FormeSleepView(labResults: model.labResults, smartWake: model.smartWake, dayKey: nil, isRoot: true)
            .formeDestinations(healthStore: healthStore)
        }
        .tag(GooseAppTab.sleep)
        .toolbar(.hidden, for: .tabBar)

        NavigationStack(path: $router.effortPath) {
          FormeEffortView(labResults: model.labResults, sport: model.sport, dayKey: nil, isRoot: true)
            .formeDestinations(healthStore: healthStore)
        }
        .tag(GooseAppTab.effort)
        .toolbar(.hidden, for: .tabBar)

        NavigationStack(path: $router.healthPath) {
          FormeIndicatorsView(labResults: model.labResults, isRoot: true)
            .formeDestinations(healthStore: healthStore)
        }
        .tag(GooseAppTab.indicators)
        .toolbar(.hidden, for: .tabBar)
      }

      // content fades out under the floating bar
      EdgeVeil(edge: .bottom, extra: 44)

      FormeTabBar(selection: tabSelection, sport: model.sport) {
        if model.sport.state == "active" || model.sport.state == "ending" { showLive = true } else { showStart = true }
      }
      .padding(.horizontal, 16)
      .padding(.bottom, 4)
    }
    .ignoresSafeArea(.keyboard)
    .sheet(isPresented: $router.showProfile) {
      NavigationStack(path: $router.morePath) {
        MoreView(healthStore: healthStore)
          .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
              Button("OK") { router.showProfile = false }.font(Forme.sans(16, .semibold))
            }
          }
      }
      .presentationDragIndicator(.visible)
    }
    .sheet(isPresented: $showStart) {
      FormeStartSheet(sport: model.sport, tracker: model.activityLocationTracker) {
        showStart = false
        showLive = true
      }
      .presentationDetents([.medium, .large])
      .presentationDragIndicator(.visible)
      .presentationCornerRadius(36)
    }
    .sheet(isPresented: $showLive) {
      FormeLiveSessionView(sport: model.sport, ble: model.ble, tracker: model.activityLocationTracker)
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(36)
    }
    // keep the analysis current instead of computing it once per launch
    .onAppear {
      healthStore.refreshPacketInputsIfNeeded()
      model.labResults.refresh()
    }
    .onReceive(model.ble.$lastHistoricalSyncCompletedAt.compactMap { $0 }) { _ in
      healthStore.refreshPacketInputsAfterCapture()
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
        withAnimation(.snappy) { router.reselect(newTab) }
        return
      }
      router.selectedTab = newTab
      model.recordUIAction("tab.selected", detail: newTab.title)
    }
  }
}

enum GooseAppTab: String, CaseIterable, Identifiable {
  case today
  case sleep
  case effort
  case indicators

  var id: String { rawValue }

  var title: String {
    switch self {
    case .today: "Aujourd'hui"
    case .sleep: "Sommeil"
    case .effort: "Effort"
    case .indicators: "Indicateurs"
    }
  }

  var systemImage: String {
    switch self {
    case .today: "house"
    case .sleep: "moon"
    case .effort: "bolt"
    case .indicators: "gauge.with.needle"
    }
  }
}

/// Floating glass tab bar + the activity button (red play; elapsed time while a session runs).
struct FormeTabBar: View {
  @Binding var selection: GooseAppTab
  @ObservedObject var sport: SportController
  let onStart: () -> Void

  private var running: Bool { sport.state == "active" || sport.state == "ending" }

  var body: some View {
    HStack(spacing: 12) {
      HStack(spacing: 0) {
        ForEach(GooseAppTab.allCases) { tab in
          Button {
            selection = tab
          } label: {
            VStack(spacing: 2) {
              Image(systemName: tab.systemImage)
                .font(.system(size: 18, weight: selection == tab ? .semibold : .regular))
                .symbolVariant(selection == tab ? .fill : .none)
                .frame(height: 22)
              Text(tab.title)
                .font(Forme.sans(10, selection == tab ? .bold : .semibold, relativeTo: .caption2))
                .lineLimit(1)
                .minimumScaleFactor(0.75)
            }
            .foregroundStyle(Forme.ink)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background {
              if selection == tab { Capsule().fill(Forme.selection).padding(.vertical, 0) }
            }
            .contentShape(Capsule())
          }
          .buttonStyle(.plain)
          .accessibilityLabel(tab.title)
          .accessibilityAddTraits(selection == tab ? .isSelected : [])
        }
      }
      .padding(5)
      .frame(height: 64)
      .glass(Capsule())
      .animation(.snappy(duration: 0.25), value: selection)

      Button(action: onStart) {
        ZStack {
          Circle().fill(Forme.strain)
          if running {
            TimelineView(.periodic(from: .now, by: 1)) { ctx in
              VStack(spacing: 0) {
                Image(systemName: "waveform.path.ecg").font(.system(size: 15, weight: .bold))
                Text(elapsed(at: ctx.date)).font(Forme.sans(11, .bold, relativeTo: .caption2)).monospacedDigit()
              }
            }
          } else {
            Image(systemName: "play.fill").font(.system(size: 21, weight: .bold)).offset(x: 1.5)
          }
        }
        .foregroundStyle(.white)
        .frame(width: 64, height: 64)
        .overlay(Circle().strokeBorder(.white.opacity(0.35), lineWidth: 1))
        .shadow(color: Forme.hex(0xb8262d, 0.30), radius: 16, x: 0, y: 12)
      }
      .buttonStyle(.plain)
      .accessibilityLabel(running ? "Séance en cours" : "Démarrer une activité")
    }
  }

  private func elapsed(at now: Date) -> String {
    let start = (sport.current["start_ts"] as? Int).map { Date(timeIntervalSince1970: TimeInterval($0)) } ?? now
    let s = max(0, Int(now.timeIntervalSince(start)))
    return s >= 3600 ? String(format: "%d:%02d", s / 3600, s / 60 % 60) : String(format: "%d:%02d", s / 60, s % 60)
  }
}

// MARK: - Navigation

enum FormeRoute: Hashable {
  case recovery(String?)
  case sleep(String?)
  case effort(String?)
  case indicators
  case session(String)
  case smartWake
  case trends
  case methods
}

private struct FormeDestinations: ViewModifier {
  @EnvironmentObject private var model: GooseAppModel
  @ObservedObject var healthStore: HealthDataStore

  func body(content: Content) -> some View {
    content
      .toolbar(.hidden, for: .tabBar)
      .navigationDestination(for: FormeRoute.self) { route in
        destination(route).toolbar(.hidden, for: .tabBar)
      }
      .navigationDestination(for: HealthRoute.self) { route in
        HealthRouteContentView(route: route, store: healthStore).toolbar(.hidden, for: .tabBar)
      }
  }

  @ViewBuilder private func destination(_ route: FormeRoute) -> some View {
        switch route {
        case .recovery(let k): FormeRecoveryView(labResults: model.labResults, dayKey: k)
        case .sleep(let k): FormeSleepView(labResults: model.labResults, smartWake: model.smartWake, dayKey: k, isRoot: false)
        case .effort(let k): FormeEffortView(labResults: model.labResults, sport: model.sport, dayKey: k, isRoot: false)
        case .indicators: FormeIndicatorsView(labResults: model.labResults, isRoot: false)
        case .session(let id): FormeSessionView(sessionID: id, sport: model.sport)
        case .smartWake: AlarmView(controller: model.smartWake, ble: model.ble)
        case .trends: FormeTrendsView(labResults: model.labResults)
        case .methods: FormeMethodsView()
        }
  }
}

extension View {
  func formeDestinations(healthStore: HealthDataStore) -> some View { modifier(FormeDestinations(healthStore: healthStore)) }
}
