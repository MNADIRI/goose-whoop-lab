import Combine
import Foundation
import SwiftUI
import UserNotifications

/// Weekly summary, Saturday -> Friday (Rust/core/src/weekly.rs): recomputed whenever the daily
/// analysis changes, shown in Indicateurs, and sent as a local notification on Friday at 19:00.
/// The pending notification is replaced by each recomputation, so it carries the data of the last
/// refresh before 19:00 (the app runs in the background with the strap).
@MainActor
final class WeeklySummaryStore: ObservableObject {
  static let shared = WeeklySummaryStore()

  @Published private(set) var summary: [String: Any] = [:]
  private weak var labResults: WhoopLabResultsStore?
  private var bag = Set<AnyCancellable>()
  private let queue = DispatchQueue(label: "weekly", qos: .utility)
  private static let enabledKey = "weekly.notification.enabled"

  var notificationsEnabled: Bool {
    get { UserDefaults.standard.object(forKey: Self.enabledKey) as? Bool ?? true }
    set {
      UserDefaults.standard.set(newValue, forKey: Self.enabledKey)
      objectWillChange.send()
      if !newValue { UNUserNotificationCenter.current().removePendingNotificationRequests(withIdentifiers: pendingIDs) }
      else { schedule() }
    }
  }

  private var pendingIDs: [String] { [(summary["notification"] as? [String: Any])?["id"] as? String].compactMap { $0 } }

  func attach(labResults: WhoopLabResultsStore) {
    self.labResults = labResults
    labResults.$raw.dropFirst().debounce(for: .seconds(5), scheduler: RunLoop.main).sink { [weak self] _ in self?.refresh() }.store(in: &bag)
    // also when the sessions change without a new analysis (detection, labels, deletions)
    Timer.publish(every: 30 * 60, tolerance: 120, on: .main, in: .common).autoconnect().sink { [weak self] _ in self?.refresh() }.store(in: &bag)
    refresh()
  }

  func refresh() {
    let args: [String: Any] = [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "analysis": labResults?.raw ?? [:],
      "utc_offset_seconds": TimeZone.current.secondsFromGMT(),
    ]
    queue.async { [weak self] in
      let r = try? GooseRustBridge().request(method: "weekly.summary", args: args)
      Task { @MainActor in
        guard let self, let r else { return }
        self.summary = r
        self.schedule()
      }
    }
  }

  /// Friday 19:00 local of the current week; nothing once that time has passed (no late duplicate).
  private func schedule() {
    guard notificationsEnabled, let n = summary["notification"] as? [String: Any], let id = n["id"] as? String,
          let at = n["at"] as? Int, let title = n["title"] as? String, let body = n["body"] as? String else { return }
    let date = Date(timeIntervalSince1970: TimeInterval(at))
    guard date > Date().addingTimeInterval(60) else { return }
    let center = UNUserNotificationCenter.current()
    center.getNotificationSettings { settings in
      guard settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional else { return }
      let content = UNMutableNotificationContent()
      content.title = title
      content.body = body
      content.sound = .default
      content.threadIdentifier = "weekly"
      content.userInfo = ["route": "weekly"]
      let comps = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute], from: date)
      center.add(UNNotificationRequest(identifier: id, content: content, trigger: UNCalendarNotificationTrigger(dateMatching: comps, repeats: false)))
    }
  }
}

/// Indicateurs card: the week (Saturday -> Friday) with the change from the previous one.
struct FormeWeeklyCard: View {
  @ObservedObject var store: WeeklySummaryStore

  private var v: BCVal { BCVal(store.summary) }

  var body: some View {
    if !store.summary.isEmpty {
      SectionTitle(title: "Semaine", subtitle: "\(v.str("week.label") ?? "") · \(v["week.complete"] as? Bool == true ? "terminée" : "en cours, samedi → vendredi")")
      FrostCard(radius: 36, padding: 20, spacing: 14) {
        LazyVGrid(columns: [GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8)], spacing: 14) {
          stat("Séances", FF.num(v.num("current.sessions.count")), "", delta("sessions.count"))
          stat("Entraînement", duration(v.num("current.sessions.minutes")), "", delta("sessions.minutes", unit: " min"))
          stat("Dépense", thousands(v.num("current.energy.total_kcal")), "kcal", nil)
          stat("Actives", thousands(v.num("current.energy.active_kcal")), "kcal", delta("energy.active_kcal"))
          stat("Sommeil", duration(v.num("current.sleep.mean_min")), "/ nuit", delta("sleep.mean_min", unit: " min"))
          stat("Récupération", FF.num(v.num("current.recovery.mean")), "/100", delta("recovery.mean"))
        }
        let types = ((store.summary["current"] as? [String: Any])?["sessions"] as? [String: Any])?["by_type"] as? [[String: Any]] ?? []
        if !types.isEmpty {
          Text(types.map { t in "\(t["count"] as? Int ?? 0) \(SportKind.of(t["type"] as? String).title.lowercased())" }.joined(separator: " · "))
            .font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
        }
        Toggle(isOn: Binding(get: { store.notificationsEnabled }, set: { store.notificationsEnabled = $0 })) {
          Text("Résumé chaque vendredi à 19 h").font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.muted)
        }
      }
      .padding(.top, 12)
    }
  }

  private func stat(_ label: String, _ value: String, _ unit: String, _ delta: String?) -> some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(label).font(Forme.sans(11, .semibold, relativeTo: .caption2)).foregroundStyle(Forme.muted).lineLimit(1).minimumScaleFactor(0.8)
      NumberUnit(value: value, unit: unit, size: 22)
      Text(delta ?? " ").font(Forme.sans(11, .semibold, relativeTo: .caption2)).monospacedDigit().foregroundStyle(Forme.muted)
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .accessibilityElement(children: .combine)
  }

  private func duration(_ m: Double?) -> String {
    guard let m else { return "—" }
    let x = Int(m.rounded())
    return x >= 60 ? "\(x / 60)h\(String(format: "%02d", x % 60))" : "\(x) min"
  }

  private func thousands(_ x: Double?) -> String {
    guard let x else { return "—" }
    return x.formatted(.number.precision(.fractionLength(0)).locale(Locale(identifier: "fr_FR")))
  }

  /// change from the previous week, only when both weeks have the value
  private func delta(_ path: String, unit: String = "") -> String? {
    guard let c = v.num("current." + path), let p = v.num("previous." + path) else { return nil }
    let d = (c - p).rounded()
    if d == 0 { return "= sem. préc." }
    return (d > 0 ? "+" : "−") + thousands(abs(d)) + unit + " vs préc."
  }
}
