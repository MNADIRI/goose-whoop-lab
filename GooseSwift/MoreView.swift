import Foundation
import CryptoKit
import SwiftUI
import UIKit

#if canImport(HealthKit)
import HealthKit
#endif

struct MoreView: View {
  @EnvironmentObject private var model: GooseAppModel
  @EnvironmentObject private var router: AppRouter
  @ObservedObject private var healthStore: HealthDataStore
  @StateObject private var store: MoreDataStore
  @AppStorage(OnboardingStorage.firstName) private var profileFirstName = ""
  @AppStorage(OnboardingStorage.unitSystem) private var profileUnitSystemRaw = "imperial"
  @AppStorage(OnboardingStorage.heightMm) private var profileHeightMm = 0
  @AppStorage(OnboardingStorage.weightGrams) private var profileWeightGrams = 0

  @MainActor
  init(healthStore: HealthDataStore) {
    self.healthStore = healthStore
    _store = StateObject(wrappedValue: MoreDataStore())
  }

  @MainActor
  init(healthStore: HealthDataStore, store: MoreDataStore) {
    self.healthStore = healthStore
    _store = StateObject(wrappedValue: store)
  }

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 0) {
        NavigationLink(value: MoreRoute.profile) {
          HStack(spacing: 14) {
            FormeDisc(systemImage: "person.fill", fg: Forme.ink, bg: Forme.sunken, size: 52, shadow: false)
            VStack(alignment: .leading, spacing: 2) {
              Text(profileFirstName.isEmpty ? "Ton profil" : profileFirstName)
                .font(Forme.sans(22, .semibold, relativeTo: .title3)).foregroundStyle(Forme.ink)
              Text(profileSummary).font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.muted)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            Image(systemName: "chevron.right").font(.system(size: 13, weight: .semibold)).foregroundStyle(Forme.muted)
          }
          .padding(18)
          .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background { FrostCard(radius: 32, padding: 0) { Color.clear } }
        .accessibilityLabel("Modifier le profil")

        group("Bracelet", MoreRoute.deviceRoutes)
        group("Application", MoreRoute.appRoutes + MoreRoute.settingsRoutes)
        group("Assistance", MoreRoute.supportRoutes + MoreRoute.developerRoutes)
      }
      .padding(.horizontal, 16)
      .padding(.top, 8)
      .padding(.bottom, 40)
      .frame(maxWidth: 560)
      .frame(maxWidth: .infinity)
    }
    .scrollIndicators(.hidden)
    .background(FormeGround())
    .navigationTitle("Profil")
    .navigationBarTitleDisplayMode(.inline)
    .toolbarBackground(.hidden, for: .navigationBar)
    .navigationDestination(for: MoreRoute.self) { route in
      destination(for: route)
    }
    .onAppear {
      model.recordUIAction("page.opened", detail: "More")
      store.refreshBridgeStatus(model: model)
      store.refreshRecentCaptureSessions()
    }
  }

  @ViewBuilder
  private func group(_ title: String, _ routes: [MoreRoute]) -> some View {
    if !routes.isEmpty {
      Text(title).font(Forme.sans(13, .bold, relativeTo: .footnote)).foregroundStyle(Forme.muted)
        .textCase(.uppercase).tracking(0.6)
        .padding(.top, 28).padding(.leading, 6)
      FrostCard(radius: 32, padding: 8, spacing: 0) {
        ForEach(Array(routes.enumerated()), id: \.element) { i, route in
          if i > 0 { FormeDivider().padding(.horizontal, 12) }
          NavigationLink(value: route) {
            HStack(spacing: 12) {
              FormeDisc(systemImage: route.systemImage, fg: Forme.ink, bg: Forme.sunken, size: 40, shadow: false)
              VStack(alignment: .leading, spacing: 1) {
                Text(Self.frTitle(route)).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
                Text(Self.frSubtitle(route)).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).lineLimit(2)
              }
              .frame(maxWidth: .infinity, alignment: .leading)
              Image(systemName: "chevron.right").font(.system(size: 13, weight: .semibold)).foregroundStyle(Forme.muted)
            }
            .padding(10)
            .contentShape(Rectangle())
          }
          .buttonStyle(.plain)
          .accessibilityLabel(Self.frTitle(route))
        }
      }
      .padding(.top, 10)
    }
  }

  static func frTitle(_ r: MoreRoute) -> String {
    switch r {
    case .profile: "Profil"
    case .device: "Bracelet"
    case .connectionLab: "Labo Bluetooth"
    case .capture: "Capture"
    case .localStore: "Stockage local"
    case .healthSync: "Apple Santé"
    case .rawExport: "Export brut"
    case .algorithms: "Algorithmes"
    case .debug: "Débogage"
    case .privacy: "Confidentialité"
    case .support: "Assistance"
    case .about: "À propos"
    case .developer: "Outils développeur"
    }
  }

  static func frSubtitle(_ r: MoreRoute) -> String {
    switch r {
    case .profile: "Prénom, naissance, taille, poids"
    case .device: "WHOOP 5.0, connexion, batterie, appairage"
    case .connectionLab: "Bluetooth bas niveau et événements"
    case .capture: "Captures, imports et preuves de commandes"
    case .localStore: "Base SQLite, schéma et état du stockage"
    case .healthSync: "Poids du profil depuis Apple Santé"
    case .rawExport: "Fenêtres, périmètres et validation"
    case .algorithms: "Préférences des algorithmes"
    case .debug: "Cœur Rust, parseur et commandes protégées"
    case .privacy: "Données locales, export et suppression"
    case .support: "Journaux, paquets d'assistance, dépannage"
    case .about: "Application, cœur Rust et licences"
    case .developer: "Captures, exports, diagnostics du pont"
    }
  }

  private var routeStatus: MoreRouteStatus {
    store.routeStatus(ble: model.ble, model: model)
  }

  @ViewBuilder
  private func routeRows(_ routes: [MoreRoute]) -> some View {
    ForEach(routes) { route in
      NavigationLink(value: route) {
        MoreRouteRow(route: route, status: routeStatus[keyPath: route.statusKeyPath])
      }
      .accessibilityLabel(route.title)
    }
  }

  @ViewBuilder
  private func destination(for route: MoreRoute) -> some View {
    switch route {
    case .device:
      DeviceView()
    case .profile:
      MoreProfileView()
    case .connectionLab:
      ConnectionView()
    case .capture:
      MoreCaptureView(store: store)
    case .localStore:
      MoreLocalStoreView(store: store)
    case .healthSync:
      MoreHealthSyncView(store: store)
    case .rawExport:
      MoreRawExportView(store: store)
    case .algorithms:
      MoreAlgorithmsView(store: store, healthStore: healthStore) {
        router.openHealth(.algorithms)
      }
    case .debug:
      MoreDebugView(store: store)
    case .privacy:
      MorePrivacyView(store: store)
    case .support:
      MoreSupportView(store: store)
    case .about:
      MoreAboutView(store: store)
    case .developer:
      MoreDeveloperView(routes: MoreRoute.developerToolRoutes, routeStatus: routeStatus)
    }
  }

  private var profileSummary: String {
    let height = MoreProfileFormatting.heightText(millimeters: profileHeightMm, unitSystemRaw: profileUnitSystemRaw)
    let weight = MoreProfileFormatting.weightText(grams: profileWeightGrams, unitSystemRaw: profileUnitSystemRaw)
    let parts = [height, weight].filter { !$0.isEmpty }
    return parts.isEmpty ? "Complète ton profil" : parts.joined(separator: " · ")
  }
}
