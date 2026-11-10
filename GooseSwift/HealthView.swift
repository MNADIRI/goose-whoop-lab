import Darwin
import Foundation
import SwiftUI
import UIKit

struct HealthView: View {
  @EnvironmentObject private var model: GooseAppModel
  @ObservedObject var store: HealthDataStore
  @ObservedObject var labResults: WhoopLabResultsStore

  var body: some View {
    ScrollView {
      LazyVStack(alignment: .leading, spacing: 22) {
        // whoop-lab: Health = history over all days; Home = one day. Same source for both.
        if let results = labResults.results {
          LabTrendsView(results: results)
        } else {
          Text(labResults.status).font(.caption).foregroundStyle(.secondary)
        }

        VStack(alignment: .leading, spacing: 0) {
          NavigationLink { LabMethodsView() } label: {
            Label("Méthodes et sources", systemImage: "function").frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 12)
          }
          Divider()
          NavigationLink(value: HealthRoute.packetInputs) {
            Label("Diagnostic : trames et pipeline goose", systemImage: "stethoscope").frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 12)
          }
        }
        .padding(.horizontal, 12)
        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 14))
      }
      .padding(.horizontal, 16)
      .padding(.vertical, 18)
    }
    .gooseScreenBackground()
    .navigationTitle("Tendances")
    .navigationBarTitleDisplayMode(.inline)
    .toolbarBackground(.hidden, for: .navigationBar)
    .navigationDestination(for: HealthRoute.self) { route in
      HealthRouteContentView(route: route, store: store)
    }
    .toolbar {
      ToolbarItem(placement: .topBarTrailing) {
        Button {
          refreshDashboard()
        } label: {
          Image(systemName: "arrow.clockwise")
        }
        .accessibilityLabel("Refresh Health")
      }
    }
    .onAppear {
      model.recordUIAction("page.opened", detail: "Health")
      store.loadBridgeCatalogsIfNeeded()
      store.refreshHeartRateTimeline()
    }
  }

  @MainActor
  private func refreshDashboard() {
    store.refreshBridgeCatalogs()
    store.refreshHeartRateTimeline()
    store.refreshPacketInputsIfNeeded()
    labResults.refresh(force: true)
  }
}
