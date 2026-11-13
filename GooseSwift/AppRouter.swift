import Foundation
import SwiftUI

@MainActor
final class AppRouter: ObservableObject {
  @Published var selectedTab: GooseAppTab = .today
  @Published var todayPath = NavigationPath()
  @Published var sleepPath = NavigationPath()
  @Published var effortPath = NavigationPath()
  /// Indicators tab: Forme routes and the technical HealthRoute pages.
  @Published var healthPath = NavigationPath()
  @Published var morePath: [MoreRoute] = []
  /// Profile, device and settings (the former "More" tab) live in a sheet.
  @Published var showProfile = false

  func openHealth(_ route: HealthRoute?) {
    showProfile = false
    selectedTab = .indicators
    var p = NavigationPath()
    if let route { p.append(route) }
    healthPath = p
  }

  func openMore(_ route: MoreRoute?) {
    morePath = route.map { [$0] } ?? []
    showProfile = true
  }

  /// Tapping the selected tab again pops it to its root.
  func popToRoot(_ tab: GooseAppTab) {
    switch tab {
    case .today: todayPath = NavigationPath()
    case .sleep: sleepPath = NavigationPath()
    case .effort: effortPath = NavigationPath()
    case .indicators: healthPath = NavigationPath()
    }
  }

  func reselect(_ tab: GooseAppTab) {
    popToRoot(tab)
  }

  @discardableResult
  func handleDeepLink(_ url: URL) -> Bool {
    if url.scheme == "gooseswift", url.host == "more" {
      let routeName = url.pathComponents.dropFirst().first ?? ""
      if routeName.isEmpty {
        openMore(nil)
        return true
      }
      guard let route = MoreRoute(rawValue: routeName) else {
        return false
      }
      openMore(route)
      return true
    }

    guard url.scheme == "gooseswift", url.host == "health" else {
      return false
    }
    let routeName = url.pathComponents.dropFirst().first ?? ""
    if routeName.isEmpty {
      openHealth(nil)
      return true
    }
    guard let route = HealthRoute(rawValue: routeName) else {
      return false
    }
    openHealth(route)
    return true
  }
}
