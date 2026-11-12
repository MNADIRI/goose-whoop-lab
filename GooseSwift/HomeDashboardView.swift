import SwiftUI

/// Home: one day at a time, from the first day with data to today. Every value comes from the
/// on-device whoop-lab analysis (WhoopLabResultsStore); see lab/METRICS.md.
struct HomeDashboardView: View {
  @EnvironmentObject private var model: GooseAppModel
  @ObservedObject var healthStore: HealthDataStore
  @ObservedObject var labResults: WhoopLabResultsStore
  @Binding var selectedDate: Date
  let openHealthRoute: (HealthRoute) -> Void
  @State private var selectedKey: String?

  var body: some View {
    ScrollView {
      LazyVStack(alignment: .leading, spacing: 22) {
        if let results = labResults.results, let current = currentDay(results) {
          dayPicker(results: results, current: current)
          LabActivityCard(day: current.day)
          LabNightCard(night: current.night)
          LabBodyCard(night: current.night)
          LabIndicesCard(day: current, circadian: results.circadian)
          Text("\(labResults.status) · \(results.device.records.formatted()) secondes enregistrées depuis le \(labDayLabel(results.firstDay ?? "", style: .dateTime.day().month(.wide)))")
            .font(.caption2)
            .foregroundStyle(.secondary)
        } else {
          ContentUnavailableView(
            labResults.isRunning ? "Analyse en cours…" : "Pas encore de données",
            systemImage: "waveform.path.ecg",
            description: Text("Porte le bracelet et laisse la synchronisation se faire. \(labResults.status)")
          )
        }
      }
      .padding(.horizontal, 16)
      .padding(.vertical, 18)
    }
    .scrollClipDisabled()
    .refreshable { labResults.refresh(force: true) }
    .onAppear {
      labResults.refresh()
      model.recordUIAction("page.opened", detail: "Home")
    }
    .gooseScreenBackground()
    .navigationTitle("Aujourd'hui")
    .navigationBarTitleDisplayMode(.inline)
    .toolbarBackground(.hidden, for: .navigationBar)
    .overlay(alignment: .top) {
      HomeTopScrollFade()
        .allowsHitTesting(false)
    }
    .safeAreaInset(edge: .bottom, alignment: .trailing) {
      HomeStartActivityFloatingButton(session: model.activitySession)
        .padding(.trailing, 18)
        .padding(.bottom, 10)
    }
    .toolbar {
      ToolbarItem(placement: .topBarTrailing) {
        NavigationLink {
          DeviceView()
        } label: {
          Image(systemName: "applewatch")
            .font(.system(size: 17, weight: .semibold))
            .foregroundStyle(deviceConnected ? .green : .red)
        }
        .accessibilityLabel("Bracelet")
        .accessibilityValue(deviceConnected ? "Connecté" : "Déconnecté")
      }
    }
  }

  private var deviceConnected: Bool {
    let state = model.ble.connectionState.lowercased()
    return state == "ready" || state == "connected"
  }

  private func currentDay(_ results: LabResults) -> LabDayResult? {
    if let key = selectedKey, let day = results.day(key) {
      return day
    }
    return results.latest
  }

  private func dayPicker(results: LabResults, current: LabDayResult) -> some View {
    let index = results.days.firstIndex { $0.date == current.date } ?? results.days.count - 1
    return HStack {
      Button {
        selectedKey = results.days[max(0, index - 1)].date
      } label: {
        Image(systemName: "chevron.left").font(.headline)
      }
      .disabled(index == 0)
      Spacer()
      VStack(spacing: 2) {
        Text(index == results.days.count - 1 ? "Aujourd'hui" : labDayLabel(current.date))
          .font(.headline)
        Text(current.night.available
             ? "Nuit du \(previousDayLabel(current.date)) au \(labDayLabel(current.date, style: .dateTime.day().month()))"
             : labDayLabel(current.date, style: .dateTime.day().month(.wide).year()))
          .font(.caption)
          .foregroundStyle(.secondary)
      }
      Spacer()
      Button {
        selectedKey = results.days[min(results.days.count - 1, index + 1)].date
      } label: {
        Image(systemName: "chevron.right").font(.headline)
      }
      .disabled(index == results.days.count - 1)
    }
  }

  private func previousDayLabel(_ key: String) -> String {
    guard let date = labDate(key), let previous = Calendar.current.date(byAdding: .day, value: -1, to: date) else { return "" }
    return previous.formatted(.dateTime.day().month())
  }
}
