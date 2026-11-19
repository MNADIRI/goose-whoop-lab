import BackgroundTasks
import Foundation
import UIKit

/// Keeps the local database small (Rust/core/src/db_maintenance.rs): bounded purges of the capture
/// layer through the day, and the one-time full VACUUM (and later ones if needed) in a background
/// processing task that iOS runs while the phone charges. Every run is short (20-50k rows) so the
/// app never holds the CPU long enough to be killed in the background.
@MainActor
final class DatabaseMaintenance {
  static let shared = DatabaseMaintenance()
  static let taskID = "com.marwannadiri.goose.db-maintenance"

  private weak var ble: GooseBLEClient?
  private weak var sport: SportController?
  private weak var smartWake: SmartWakeController?
  private let queue = DispatchQueue(label: "db-maintenance", qos: .utility)
  private var running = false
  private var timer: Timer?
  private(set) var last: [String: Any] = [:]

  /// Registered before the app finishes launching (GooseSwiftApp.init).
  nonisolated static func register() {
    BGTaskScheduler.shared.register(forTaskWithIdentifier: taskID, using: nil) { task in
      guard let task = task as? BGProcessingTask else { return }
      Task { @MainActor in DatabaseMaintenance.shared.runProcessingTask(task) }
    }
  }

  func attach(ble: GooseBLEClient, sport: SportController, smartWake: SmartWakeController) {
    self.ble = ble
    self.sport = sport
    self.smartWake = smartWake
    schedule(after: 60)
    scheduleProcessingTask()
  }

  private var inBackground: Bool { UIApplication.shared.applicationState == .background }

  private func schedule(after seconds: TimeInterval) {
    timer?.invalidate()
    timer = Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { [weak self] _ in
      Task { @MainActor in self?.runLight() }
    }
    timer?.tolerance = seconds * 0.2
  }

  /// one bounded purge (+ incremental page return), then the next one: soon while a backlog
  /// remains, every 10-20 min otherwise
  private func runLight() {
    guard !running else { return schedule(after: 60) }
    running = true
    let background = inBackground
    let args: [String: Any] = [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "max_rows": background ? 4_000 : 10_000,
      "vacuum": "incremental",
    ]
    queue.async { [weak self] in
      let started = Date()
      let r = try? GooseRustBridge().request(method: "db.maintain", args: args)
      let seconds = Date().timeIntervalSince(started)
      Task { @MainActor in
        guard let self else { return }
        self.running = false
        if let r { self.last = r }
        let backlog = r?["backlog"] as? Bool ?? false
        self.ble?.record(source: "db.maintenance", title: "light", body: String(format: "%.1f s ", seconds) + "\(r?["deleted"] ?? "failed") file=\(r?["file_bytes"] ?? "?") free=\(r?["free_bytes"] ?? "?")")
        if r?["full_vacuum_due"] as? Bool == true { self.scheduleProcessingTask() }
        self.schedule(after: backlog ? (self.inBackground ? 180 : 30) : (self.inBackground ? 1200 : 600))
      }
    }
  }

  private func scheduleProcessingTask() {
    let request = BGProcessingTaskRequest(identifier: Self.taskID)
    request.requiresExternalPower = true
    request.requiresNetworkConnectivity = false
    request.earliestBeginDate = Date().addingTimeInterval(15 * 60)
    try? BGTaskScheduler.shared.submit(request)
  }

  /// While charging: the whole backlog, then a full VACUUM (the database is rewritten without its
  /// free pages and switched to incremental mode). History syncs wait meanwhile; nothing runs while
  /// a sport session or a Smart Wake night owns the live streams.
  private func runProcessingTask(_ task: BGProcessingTask) {
    scheduleProcessingTask()
    let busy = (sport?.state ?? "idle") != "idle" || smartWake?.session != nil
    guard !busy else { task.setTaskCompleted(success: true); return }
    var cancelled = false
    task.expirationHandler = { cancelled = true }
    ble?.syncHoldUntil = Date().addingTimeInterval(10 * 60)
    let path = HealthDataStore.defaultDatabasePath()
    queue.async { [weak self] in
      var r: [String: Any]?
      for _ in 0..<400 where !cancelled {
        r = try? GooseRustBridge().request(method: "db.maintain", args: ["database_path": path, "max_rows": 100_000, "vacuum": "none"])
        if r?["backlog"] as? Bool != true { break }
      }
      if !cancelled, r?["full_vacuum_due"] as? Bool == true {
        r = try? GooseRustBridge().request(method: "db.maintain", args: ["database_path": path, "max_rows": 100_000, "vacuum": "full"])
      }
      Task { @MainActor in
        self?.ble?.syncHoldUntil = nil
        if let r { self?.last = r }
        self?.ble?.record(source: "db.maintenance", title: "processing_task", body: "vacuum=\(r?["vacuum"] ?? "-") file=\(r?["file_bytes"] ?? "?") cancelled=\(cancelled)")
        task.setTaskCompleted(success: !cancelled)
      }
    }
  }
}
