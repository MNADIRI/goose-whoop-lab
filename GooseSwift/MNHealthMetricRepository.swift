import Combine
import Foundation
import WidgetKit

/// Collects what the daily widget needs from the app's existing producers and hands it to the
/// Rust engine (`daily.snapshot`), which does all the evaluation (quality, context, schedule,
/// insight). Nothing is scored here.
///
/// Inputs: the analysis already held by `WhoopLabResultsStore` (`whoop_lab.analyze`), the Smart
/// Wake configuration (authoritative, read-only), the latest heart rate seen over BLE with its
/// timestamp, and the activity currently detected by the sport engine. The engine adds the sleep
/// observation, the newest synced heart rate and tonight's plan from the database.
@MainActor
final class MNHealthMetricRepository {
  private weak var labResults: WhoopLabResultsStore?
  private weak var smartWake: SmartWakeController?
  private weak var sport: SportController?
  private weak var ble: GooseBLEClient?
  private let queue = DispatchQueue(label: "mn.daily-snapshot", qos: .utility)
  private var bag = Set<AnyCancellable>()
  private var timer: Timer?
  private var running = false
  private var pending = false
  private var lastReload = Date.distantPast
  private var lastContext: DailyContext?
  private var lastHero = ""

  /// widget reloads are budgeted by iOS: at most one every 15 min unless something important changed
  private static let reloadInterval: TimeInterval = 15 * 60

  func attach(labResults: WhoopLabResultsStore, smartWake: SmartWakeController, sport: SportController, ble: GooseBLEClient) {
    self.labResults = labResults
    self.smartWake = smartWake
    self.sport = sport
    self.ble = ble
    labResults.$raw.dropFirst().debounce(for: .seconds(2), scheduler: RunLoop.main).sink { [weak self] _ in self?.refresh(reason: "analysis") }.store(in: &bag)
    smartWake.$session.dropFirst().sink { [weak self] _ in self?.refresh(reason: "alarm", important: true) }.store(in: &bag)
    smartWake.$engineState.dropFirst().removeDuplicates().sink { [weak self] _ in self?.refresh(reason: "wake_state", important: true) }.store(in: &bag)
    smartWake.$confirmedAwakeAt.dropFirst().sink { [weak self] _ in self?.refresh(reason: "awake", important: true) }.store(in: &bag)
    smartWake.$latest.dropFirst().debounce(for: .seconds(3), scheduler: RunLoop.main).sink { [weak self] _ in self?.refresh(reason: "alarm_time") }.store(in: &bag)
    sport.$state.dropFirst().removeDuplicates().sink { [weak self] _ in self?.refresh(reason: "activity", important: true) }.store(in: &bag)
    // live HR: the engine shows it with its time; no need to follow every beat
    ble.$liveHeartRateUpdatedAt.dropFirst().throttle(for: .seconds(600), scheduler: RunLoop.main, latest: true).sink { [weak self] _ in self?.refresh(reason: "hr") }.store(in: &bag)
    let t = Timer(timeInterval: 15 * 60, repeats: true) { [weak self] _ in Task { @MainActor in self?.refresh(reason: "timer") } }
    t.tolerance = 120
    RunLoop.main.add(t, forMode: .common)
    timer = t
    refresh(reason: "launch", important: true)
  }

  private func inputs() -> [String: Any] {
    var args: [String: Any] = [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "now_unix": Int(Date().timeIntervalSince1970),
      "utc_offset_seconds": TimeZone.current.secondsFromGMT(),
      "analysis": labResults?.raw ?? [:],
    ]
    if let sw = smartWake {
      sw.alignWakeWithDayType()
      var cfg: [String: Any] = [
        "armed": sw.session != nil,
        "deadline": Int(sw.deadline.timeIntervalSince1970),
        "max_advance_min": sw.session.map { $0.maxAdvanceMin ?? 30 } ?? sw.effectiveMaxAdvance,
        "state": sw.engineState,
      ]
      if let a = sw.confirmedAwakeAt { cfg["confirmed_awake_at"] = Int(a.timeIntervalSince1970) }
      args["smart_wake"] = cfg
    }
    if let ble, let bpm = ble.liveHeartRateBPM, let at = ble.liveHeartRateUpdatedAt {
      args["live_hr"] = ["bpm": bpm, "ts": Int(at.timeIntervalSince1970), "source": ble.liveHeartRateSource]
    }
    if let sport, sport.state != "idle", let start = sport.current["start_ts"] as? Int {
      let label = sport.current["label_type"] as? String
      let kind = SportKind.of(label ?? sport.current["guess"] as? String)
      args["activity"] = ["kind": label ?? sport.current["guess"] as? String ?? "séance", "title": kind == .course ? "Course" : kind.title, "start_ts": start, "confirmed": label != nil]
    }
    if let prev = DailyHealthSnapshotStore.load(), Date().timeIntervalSince(prev.generatedDate) < 14 * 3600 {
      args["previous"] = ["context": prev.context.rawValue, "since": prev.contextSince]
    }
    return args
  }

  func refresh(reason: String, important: Bool = false) {
    guard !running else { pending = true; return }
    running = true
    let args = inputs()
    queue.async { [weak self] in
      let result = try? GooseRustBridge().request(method: "daily.snapshot", args: args)
      let data = result.flatMap { try? JSONSerialization.data(withJSONObject: $0) }
      Task { @MainActor in
        guard let self else { return }
        self.running = false
        if let data, let snap = DailyHealthSnapshotStore.save(json: data) {
          self.reloadIfNeeded(snap, important: important)
        }
        if self.pending {
          self.pending = false
          self.refresh(reason: "pending")
        }
      }
    }
  }

  private func reloadIfNeeded(_ snap: DailyHealthSnapshot, important: Bool) {
    let hero = snap.entry(at: Date())?.card.heroMetric?.display ?? ""
    let changed = snap.context != lastContext || hero != lastHero
    guard important || changed || Date().timeIntervalSince(lastReload) >= Self.reloadInterval else { return }
    lastContext = snap.context
    lastHero = hero
    lastReload = Date()
    WidgetCenter.shared.reloadTimelines(ofKind: DailyHealthSnapshotStore.widgetKind)
  }
}
